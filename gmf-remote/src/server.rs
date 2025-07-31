use std::path::PathBuf;
use std::sync::Arc;

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose;
use futures::stream::{self, StreamExt};
use gmf_common::consts::NONCE_SIZE;
use gmf_common::interface::*;
use gmf_common::r2::{init_s3_client, put_object};
use std::time::Instant;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, BufReader},
    sync::{Mutex, mpsc},
};

#[derive(Debug, Clone)]
pub struct TaskMetadata {
    pub file_path: PathBuf,
    pub file_name: String,
    pub file_size: u64,
    pub chunk_size: u64,
    pub total_chunks: u64,
    pub concurrency: u64,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub metadata: Option<TaskMetadata>,
}

pub type SharedState = Arc<Mutex<AppState>>;

pub async fn handle_message(
    message: Message,
    state: SharedState,
    sender: mpsc::Sender<Message>,
) -> Result<()> {
    if let Message::Request(request) = message {
        match request {
            ClientRequest::Setup(payload) => {
                let result = setup(payload, state).await;
                let response_message = match result {
                    Ok(res) => res.into(),
                    Err(e) => ServerResponse::Error(format!("Setup 失败: {}", e)).into(),
                };
                sender.send(response_message).await?;
            }
            ClientRequest::Start(payload) => {
                let ack = ServerResponse::StartSuccess;
                sender.send(ack.into()).await?;

                // 开始分块加密上传
                tokio::spawn(start(payload, state, sender.clone()));
            }
        }
    } else {
        let error_response = ServerResponse::InvalidRequest(
            "协议错误：客户端不应向服务端发送此类型的消息。".to_string(),
        );
        let _ = sender.send(error_response.into()).await;
    }
    Ok(())
}

pub async fn setup(payload: SetupRequestPayload, state: SharedState) -> Result<ServerResponse> {
    if payload.chunk_size == 0 {
        return Err(anyhow!("chunk_size 不能为 0"));
    }

    init_s3_client(None).await?;

    let file_path: PathBuf = shellexpand::tilde(&payload.path).to_string().parse()?;
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("无法提取文件名")?
        .to_string();
    let file_size = tokio::fs::metadata(&file_path).await?.len();
    let total_chunks = (file_size as f64 / payload.chunk_size as f64).ceil() as u64;

    let task_metadata = TaskMetadata {
        file_path,
        file_name: file_name.clone(),
        file_size,
        chunk_size: payload.chunk_size,
        total_chunks,
        concurrency: payload.concurrency,
    };

    {
        let mut app_state = state.lock().await;
        app_state.metadata = Some(task_metadata);
    }

    // --- 构造成功响应 ---
    let response_data = ServerResponse::SetupSuccess(SetupResponse {
        file_name,
        file_size,
        total_chunks,
    });

    Ok(response_data)
}

pub async fn start(
    payload: StartRequestPayload,
    state: SharedState,
    sender: mpsc::Sender<Message>,
) -> Result<()> {
    let resume_from_chunk_id = payload.resume_from_chunk_id;
    // 1. 初始化
    let (metadata, chunk_size, concurrency) = {
        let app_state = state.lock().await;
        if let Some(metadata) = app_state.metadata.clone() {
            let chunk_size = metadata.chunk_size;
            let concurrency = metadata.concurrency;
            (metadata, chunk_size, concurrency)
        } else {
            // 如果元数据不存在，发送错误并返回
            let error_response =
                ServerResponse::Error("任务未初始化。请在 'start' 之前先调用 'setup'".to_string());
            let _ = sender.send(error_response.into()).await;
            return Ok(());
        }
    };

    // 处理断点续传的 seek 逻辑
    let mut file = File::open(&metadata.file_path).await?;
    if resume_from_chunk_id > 0 {
        let offset = resume_from_chunk_id * chunk_size;
        file.seek(tokio::io::SeekFrom::Start(offset)).await?;
    }

    let reader = BufReader::new(file);
    let file_reader_stream = stream::unfold(
        (reader, resume_from_chunk_id),
        |(mut reader, chunk_id)| async move {
            let mut buf = vec![0u8; chunk_size as usize];
            let mut filled: usize = 0;

            // read 不保证每次都读取 CHUNK_SIZE 字节，因此需要循环读取直到填满或 EOF
            loop {
                if filled == chunk_size as usize {
                    break;
                }
                match reader.read(&mut buf[filled..]).await {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => {
                        return None;
                    }
                }
            }

            if filled == 0 {
                None // EOF 且没有数据 -> 结束
            } else {
                buf.truncate(filled);
                Some((
                    ChunkJob {
                        id: chunk_id,
                        data: buf,
                    },
                    (reader, chunk_id + 1),
                ))
            }
        },
    );

    // 3. 并发调度与执行，并收集所有结果
    file_reader_stream
        .map(|job: ChunkJob| process_single_chunk(job, sender.clone()))
        .buffered(concurrency as usize)
        .for_each(|_| async { /* 忽略结果，任务会在内部处理 */ })
        .await;

    let completed_response = ServerResponse::Completed;
    sender.send(completed_response.into()).await?;

    Ok(())
}

pub struct ChunkJob {
    pub id: u64,
    pub data: Vec<u8>,
}

async fn process_single_chunk(job: ChunkJob, sender: mpsc::Sender<Message>) {
    let chunk_id = job.id;

    let result: Result<(), anyhow::Error> = async {

        // 加密
        let (encrypted_data, passphrase_b64) =
            encrypt_chunk(&job.data).with_context(|| format!("分块 #{} 加密失败", chunk_id))?;

        // 上传
        upload_chunk(chunk_id, encrypted_data)
            .await
            .with_context(|| format!("分块 #{} 上传失败", chunk_id))?;

        // 发送成功消息
        let ready = ServerResponse::ChunkReadyForDownload {
            chunk_id,
            passphrase_b64,
        };
        sender
            .send(ready.into())
            .await
            .map_err(|e| anyhow!("无法向客户端发送成功消息: {}", e))?;

        Ok(())
    }
    .await;

    if let Err(e) = result {

        let fatal_error_response =
            ServerResponse::FatalError(format!("处理分块 #{} 时发生错误: {}", chunk_id, e));

        if sender.send(fatal_error_response.into()).await.is_err() {
            eprintln!(
                "[ERROR] Channel closed. Could not send FatalError for chunk #{}.",
                chunk_id
            );
        }
    }
}

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

// TODO: 超时与错误重试（达到重试次数则退出）
async fn upload_chunk(chunk_id: u64, data: Vec<u8>) -> Result<()> {
    let start = Instant::now();
    put_object(&chunk_id.to_string(), data).await?;
    let duration = start.elapsed();
    Ok(())
}
