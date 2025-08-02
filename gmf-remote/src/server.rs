use std::{path::PathBuf, sync::Arc};

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose};
use futures::{StreamExt, TryStreamExt, stream};
use gmf_common::{
    consts::NONCE_SIZE,
    interface::*,
    r2::{init_s3_client, put_object},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, BufReader},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tracing::{error, info};

#[derive(Debug, Clone)]
pub struct TaskMetadata {
    pub file_path: PathBuf,
    pub chunk_size: u64,
    pub concurrency: u64,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub metadata: Option<TaskMetadata>,
}

pub type SharedState = Arc<Mutex<AppState>>;

pub struct ChunkJob {
    pub id: u64,
    pub data: Vec<u8>,
}

pub async fn handle_message(
    message: Message,
    state: SharedState,
    sender: mpsc::Sender<Message>,
) -> Result<Option<JoinHandle<()>>> {
    let request = match message {
        Message::Request(req) => req,
        _ => {
            let error_response = ServerResponse::InvalidRequest(
                "协议错误：收到了非 Request 类型的消息。".to_string(),
            );
            if sender.send(error_response.into()).await.is_err() {
                error!("Channel closed. Could not send InvalidRequest error.");
            }
            return Ok(None);
        }
    };

    match request {
        ClientRequest::Setup(payload) => {
            let response = setup(payload, state).await;
            if sender.send(response.into()).await.is_err() {
                error!("Channel closed. Could not send setup response.");
            }
            Ok(None)
        }
        ClientRequest::Start(payload) => {
            let ack = ServerResponse::StartSuccess;
            if sender.send(ack.into()).await.is_err() {
                error!("Channel closed. Could not send StartSuccess ack.");
                return Ok(None);
            }
            let handle = tokio::spawn(start(payload, state, sender.clone()));
            Ok(Some(handle))
        }
    }
}

pub async fn setup(payload: SetupRequestPayload, state: SharedState) -> ServerResponse {
    if payload.chunk_size == 0 {
        return ServerResponse::InvalidRequest("chunk_size 不能为 0".to_string());
    }
    if payload.concurrency == 0 {
        return ServerResponse::InvalidRequest("concurrency 不能为 0".to_string());
    }

    if let Err(e) = init_s3_client(None).await {
        return ServerResponse::Error(format!("S3 客户端初始化失败: {e}"));
    }

    let file_path_str = shellexpand::tilde(&payload.path).to_string();
    let file_path: PathBuf = match file_path_str.parse() {
        Ok(p) => p,
        Err(_) => {
            return ServerResponse::InvalidRequest(format!(
                "提供的文件路径无效: '{}'",
                payload.path
            ));
        }
    };

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());

    if file_name.is_none() {
        return ServerResponse::InvalidRequest(format!(
            "无法从路径 '{}' 中提取有效的文件名",
            payload.path
        ));
    }

    // 尝试获取文件元数据，如果文件不存在，返回 NotFound
    let metadata = match tokio::fs::metadata(&file_path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ServerResponse::NotFound(format!("文件未找到: '{}'", file_path.display()));
        }
        Err(e) => {
            // 其他文件系统错误（如权限问题）
            return ServerResponse::Error(format!("无法访问文件 '{}': {}", file_path.display(), e));
        }
    };

    let file_size = metadata.len();
    let total_chunks = (file_size as f64 / payload.chunk_size as f64).ceil() as u64;

    let task_metadata = TaskMetadata {
        file_path,
        chunk_size: payload.chunk_size,
        concurrency: payload.concurrency,
    };

    {
        let mut app_state = state.lock().await;
        app_state.metadata = Some(task_metadata);
    }

    ServerResponse::SetupSuccess(SetupResponse {
        file_name: file_name.unwrap(),
        file_size,
        total_chunks,
    })
}

pub async fn start(
    payload: StartRequestPayload,
    state: SharedState,
    sender: mpsc::Sender<Message>,
) {
    let result: Result<()> = async {
        let (file_path, chunk_size, concurrency) = {
            let app_state = state.lock().await;
            let metadata = app_state
                .metadata
                .as_ref()
                .context("任务未初始化。请在 'start' 之前先调用 'setup'")?;
            (
                metadata.file_path.clone(),
                metadata.chunk_size,
                metadata.concurrency,
            )
        };

        let mut file = File::open(&file_path)
            .await
            .with_context(|| format!("无法打开文件: {}", file_path.display()))?;

        if payload.resume_from_chunk_id > 0 {
            let offset = payload.resume_from_chunk_id * chunk_size;
            file.seek(tokio::io::SeekFrom::Start(offset))
                .await
                .with_context(|| format!("无法跳转到文件偏移量 {offset}"))?;
        }

        let reader = BufReader::new(file);

        let file_reader_stream = stream::try_unfold(
            (reader, payload.resume_from_chunk_id),
            move |(mut reader, chunk_id)| async move {
                let mut buf = vec![0u8; chunk_size as usize];
                let mut filled = 0;
                loop {
                    if filled == chunk_size as usize {
                        break;
                    }
                    match reader.read(&mut buf[filled..]).await {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) => return Err(e),
                    }
                }
                if filled == 0 {
                    Ok(None)
                } else {
                    buf.truncate(filled);
                    Ok(Some((
                        ChunkJob {
                            id: chunk_id,
                            data: buf,
                        },
                        (reader, chunk_id + 1),
                    )))
                }
            },
        );

        file_reader_stream
            .map_err(anyhow::Error::from)
            .map(|job_result| async {
                let job = job_result?;
                process_single_chunk(job, sender.clone()).await
            })
            .buffered(concurrency as usize)
            .try_for_each(|_| async { Ok(()) })
            .await
            .context("文件流处理过程中发生错误")?;

        let completed_response = ServerResponse::UploadCompleted;
        sender
            .send(completed_response.into())
            .await
            .context("无法发送 UploadCompleted 消息")?;

        Ok(())
    }
    .await;

    // 如果上述任何步骤失败，发送一个 Error 消息
    if let Err(e) = result {
        let error_chain = e
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("\n  caused by: ");
        let error_message = format!("上传任务执行失败:\n{error_chain}");
        let error_response = ServerResponse::Error(error_message);
        if sender.send(error_response.into()).await.is_err() {
            error!(
                "Channel closed. Could not send Error for start task. Final error was: {:?}",
                e
            );
        }
    }
}

async fn process_single_chunk(job: ChunkJob, sender: mpsc::Sender<Message>) -> Result<()> {
    let start_time = std::time::Instant::now();
    let chunk_id = job.id;
    info!("开始处理分块 #{chunk_id}...");

    let (encrypted_data, passphrase_b64) =
        encrypt_chunk(&job.data).with_context(|| format!("分块 #{chunk_id} 加密失败"))?;

    put_object(&chunk_id.to_string(), encrypted_data)
        .await
        .with_context(|| format!("分块 #{chunk_id} 上传失败 (已自动重试)"))?;

    let success_response = ServerResponse::ChunkReadyForDownload {
        chunk_id,
        passphrase_b64,
    };

    sender
        .send(success_response.into())
        .await
        .map_err(|e| anyhow!("无法将 ChunkReadyForDownload 消息发送给客户端: {}", e))?;

    info!(
        "分块 #{chunk_id} 处理成功, 耗时: {} s",
        start_time.elapsed().as_secs_f32()
    );
    Ok(())
}

// TODO：由于加密导致的文件大小增加量不正常
fn encrypt_chunk(input_data: &[u8]) -> anyhow::Result<(Vec<u8>, String)> {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);

    // 生成随机 nonce（每个文件都唯一）
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = Key::<Aes256Gcm>::from_slice(&key);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, input_data)
        .map_err(|e| anyhow::anyhow!("加密失败: {:?}", e))?;

    // 输出格式：nonce || ciphertext
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes[..]);
    result.extend_from_slice(&ciphertext);

    Ok((result, general_purpose::STANDARD.encode(key)))
}
