use crate::metadata::{self, TaskMetadata, global_chunk_bitmap, global_metadata};
use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose};
use gmf_common::{consts::NONCE_SIZE, interface::*, r2::put_object, utils::calc_xxh3};
use std::path::PathBuf;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
    sync::mpsc,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub async fn setup(path: String, chunk_size: u64) -> ServerResponse {
    if chunk_size == 0 {
        return ServerResponse::Error("chunk_size 不能为 0".to_string());
    }

    // 进行一系列的文件（路径）检查
    let file_path_str = shellexpand::tilde(&path).to_string();
    let file_path: PathBuf = match file_path_str.parse() {
        Ok(p) => p,
        Err(_) => {
            return ServerResponse::Error(format!("提供的文件路径无效: '{}'", path));
        }
    };

    let file_name = match file_path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.to_string(),
        None => {
            return ServerResponse::Error(format!("无法从路径 '{}' 中提取有效的文件名", path));
        }
    };

    let file_metadata = match tokio::fs::metadata(&file_path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ServerResponse::Error(format!("文件未找到: '{}'", file_path.display()));
        }
        Err(e) => {
            return ServerResponse::Error(format!("无法访问文件 '{}': {}", file_path.display(), e));
        }
    };

    if file_metadata.is_dir() {
        return ServerResponse::Error(format!(
            "路径指向一个目录，而不是文件: '{}'",
            file_path.display()
        ));
    }

    let file_size = file_metadata.len();
    let total_chunks = (file_size as f64 / chunk_size as f64).ceil() as u64;
    let xxh3 = calc_xxh3(&file_path).unwrap();

    metadata::init_metadata(TaskMetadata::new(
        file_path.clone(),
        file_size,
        chunk_size,
        total_chunks,
    ));

    ServerResponse::SetupSuccess(SetupResponse {
        file_name,
        file_size,
        total_chunks,
        xxh3,
    })
}

pub async fn start(sender: mpsc::Sender<Message>, shutdown: CancellationToken) {
    let result: Result<()> = async {
        // 1. 读取静态元数据
        let meta = global_metadata();
        let file_path: PathBuf = meta.file_path().clone();
        let chunk_size = meta.chunk_size();
        let total_chunks = meta.total_chunks();

        // 2. 从全局 bitmap 里拿到所有未完成分块
        let pending_ids: Vec<u64> = {
            let bitmap = global_chunk_bitmap().await;
            let pending = bitmap.pending_ids();
            info!(
                "当前任务总分块数: {}, 未完成分块数: {}, 未完成ID: {:?}",
                total_chunks,
                pending.len(),
                pending
            );
            pending
        };

        // 3. 用 JoinSet 做一个简单的并发调度器（最多 5 个同时运行）
        const MAX_CONCURRENCY: usize = 5;
        let mut join_set = JoinSet::new();
        let mut in_flight = 0usize;
        let mut iter = pending_ids.into_iter();

        // 先预热最多 5 个任务
        while in_flight < MAX_CONCURRENCY {
            if let Some(chunk_id) = iter.next() {
                let file_path_clone = file_path.clone();
                let sender_clone = sender.clone();
                let shutdown_clone = shutdown.clone();

                join_set.spawn(async move {
                    if let Err(e) = upload_chunk(
                        chunk_id,
                        file_path_clone,
                        chunk_size,
                        sender_clone,
                        shutdown_clone,
                    )
                    .await
                    {
                        error!("分块 #{chunk_id} 上传失败: {e:?}");
                    }
                });

                in_flight += 1;
            } else {
                break;
            }
        }

        // 不断从 JoinSet 里回收已完成任务，并按需补充新任务
        while in_flight > 0 {
            // 如果触发了 shutdown，就不再启动新的任务，只等在跑的那几个结束
            if shutdown.is_cancelled() {
                info!("收到关闭信号，停止启动新的分块上传任务");
            }

            match join_set.join_next().await {
                Some(join_res) => {
                    in_flight -= 1;

                    if let Err(e) = join_res {
                        if e.is_cancelled() {
                            warn!("某个分块任务在关闭过程中被中止: {e:?}");
                        } else if e.is_panic() {
                            error!("某个分块任务发生 panic: {e:?}");
                        } else {
                            warn!("某个分块任务以未知方式结束: {e:?}");
                        }
                    }

                    // 如果还没 shutdown，并且还有待上传的分块，则补充一个新任务上来
                    if !shutdown.is_cancelled() {
                        if let Some(chunk_id) = iter.next() {
                            let file_path_clone = file_path.clone();
                            let sender_clone = sender.clone();
                            let shutdown_clone = shutdown.clone();

                            join_set.spawn(async move {
                                if let Err(e) = upload_chunk(
                                    chunk_id,
                                    file_path_clone,
                                    chunk_size,
                                    sender_clone,
                                    shutdown_clone,
                                )
                                .await
                                {
                                    error!("分块 #{chunk_id} 上传失败: {e:?}");
                                }
                            });

                            in_flight += 1;
                        }
                    }
                }
                None => {
                    break;
                }
            }
        }

        if shutdown.is_cancelled() {
            info!("start 退出，未完成所有分块上传");
        } else {
            info!("所有分块上传完成");
            sender
                .send(ServerResponse::UploadCompleted.into())
                .await
                .context("无法发送 UploadCompleted 消息")?;
        }

        Ok(())
    }
    .await;

    // 统一错误出口
    if let Err(e) = result {
        let error_chain = e
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("\n  caused by: ");
        let error_message = format!("start 上传任务执行失败:\n{error_chain}");
        let error_response = ServerResponse::Error(error_message);
        if sender.send(error_response.into()).await.is_err() {
            error!(
                "Channel closed. Could not send Error for start task. Final error was: {:?}",
                e
            );
        }
    }
}

async fn upload_chunk(
    chunk_id: u64,
    file_path: PathBuf,
    chunk_size: u64,
    sender: mpsc::Sender<Message>,
    shutdown: CancellationToken,
) -> Result<()> {
    // 1. 读取文件对应分块
    let mut file = File::open(&file_path)
        .await
        .with_context(|| format!("无法打开文件: {}", file_path.display()))?;

    let offset = chunk_id * chunk_size;
    file.seek(tokio::io::SeekFrom::Start(offset))
        .await
        .with_context(|| format!("无法跳转到文件偏移量 {offset} (分块 #{chunk_id})"))?;

    let mut chunk_data = Vec::with_capacity(chunk_size as usize);
    let bytes_read = file
        .take(chunk_size)
        .read_to_end(&mut chunk_data)
        .await
        .with_context(|| format!("读取分块 #{chunk_id} 的数据时失败"))?;

    if bytes_read == 0 {
        return Err(anyhow!(
            "指定的块号 {} 超出文件范围，没有可读取的数据。",
            chunk_id
        ));
    }

    info!("分块 #{chunk_id} 已从文件读取, 大小: {} 字节", bytes_read);

    // 2. 加密+上传
    tokio::select! {
        _ = shutdown.cancelled() => {
            info!("分块 #{chunk_id} 在处理过程中收到关闭信号，中止该分块处理");
            return Ok(());
        }

        res = async {
            // 2.1 加密
            let encrypt_start = std::time::Instant::now();
            let (encrypted_data, passphrase_b64) =
                encrypt_chunk(&chunk_data).with_context(|| format!("分块 #{chunk_id} 加密失败"))?;
            info!(
                "分块 #{chunk_id} 加密成功, 耗时: {} s",
                encrypt_start.elapsed().as_secs_f32()
            );

            // 2.2 上传
            let upload_start = std::time::Instant::now();
            put_object(&chunk_id.to_string(), &encrypted_data)
                .await
                .with_context(|| format!("分块 #{chunk_id} 上传失败 (已自动重试)"))?;
            info!(
                "分块 #{chunk_id} 上传成功, 耗时: {} s",
                upload_start.elapsed().as_secs_f32()
            );

            // 2.3 通知客户端该分块可用
            let success_response = ServerResponse::ChunkReadyForDownload {
                chunk_id,
                passphrase_b64,
            };
            sender
                .send(success_response.into())
                .await
                .context("无法将 ChunkReadyForDownload 消息发送给客户端: 通道已关闭")?;

            Ok::<(), anyhow::Error>(())
        } => {
            res?;
        }
    }

    Ok(())
}

fn encrypt_chunk(input_data: &[u8]) -> Result<(Vec<u8>, String)> {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);

    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = Key::<Aes256Gcm>::from_slice(&key);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, input_data)
        .map_err(|e| anyhow!("加密失败: {:?}", e))?;

    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes[..]);
    result.extend_from_slice(&ciphertext);

    Ok((result, general_purpose::STANDARD.encode(key)))
}
