use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
};
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose};
use futures::future::{AbortHandle, Abortable};
use futures::{TryStreamExt, stream};
use gmf_common::{consts::NONCE_SIZE, interface::*, r2::put_object, utils::calc_xxh3};
use std::{path::PathBuf, sync::Arc};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, BufReader},
    sync::{Mutex, mpsc, watch},
    task::JoinHandle,
};
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct TaskMetadata {
    pub file_path: PathBuf,
    pub file_size: u64,
    pub chunk_size: u64,
    #[allow(unused)]
    pub total_chunks: u64,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub metadata: Option<TaskMetadata>,
}

pub type SharedState = Arc<Mutex<AppState>>;

struct EncryptedChunk {
    id: u64,
    data: Vec<u8>,
    passphrase_b64: String,
}

pub async fn handle_message(
    request: ClientRequest,
    state: SharedState,
    sender: mpsc::Sender<Message>,
    shutdown_rx: watch::Receiver<()>,
) -> Result<Option<JoinHandle<()>>> {
    match request {
        ClientRequest::Setup { path, chunk_size } => {
            let response = setup(path, chunk_size, state).await;
            if sender.send(response.into()).await.is_err() {
                error!("Channel closed. Could not send setup response.");
            }
            Ok(None)
        }
        ClientRequest::Start {
            resume_from_chunk_id,
        } => {
            let (file_size, chunk_size) = {
                let app_state = state.lock().await;
                let metadata = app_state
                    .metadata
                    .as_ref()
                    .context("任务未初始化。请在 'start' 之前先调用 'setup'")?;
                (metadata.file_size, metadata.chunk_size)
            };
            let sent_bytes = resume_from_chunk_id * chunk_size;
            let remaining_size = file_size.saturating_sub(sent_bytes);
            let ack = ServerResponse::StartSuccess(StartResponse { remaining_size });
            if sender.send(ack.into()).await.is_err() {
                error!("Channel closed. Could not send StartSuccess ack.");
                return Ok(None);
            }
            let handle = tokio::spawn(start(
                resume_from_chunk_id,
                state,
                sender.clone(),
                shutdown_rx,
            ));
            Ok(Some(handle))
        }
        ClientRequest::Retry { chunk_id } => {
            tokio::spawn(chunk_retry(chunk_id, state, sender.clone()));
            Ok(None)
        }
    }
}

pub async fn setup(path: String, chunk_size: u64, state: SharedState) -> ServerResponse {
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

    let metadata = match tokio::fs::metadata(&file_path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ServerResponse::Error(format!("文件未找到: '{}'", file_path.display()));
        }
        Err(e) => {
            return ServerResponse::Error(format!("无法访问文件 '{}': {}", file_path.display(), e));
        }
    };

    if metadata.is_dir() {
        return ServerResponse::Error(format!(
            "路径指向一个目录，而不是文件: '{}'",
            file_path.display()
        ));
    }

    let file_size = metadata.len();
    let total_chunks = (file_size as f64 / chunk_size as f64).ceil() as u64;
    let xxh3 = calc_xxh3(&file_path).unwrap();

    let task_metadata = TaskMetadata {
        file_path,
        chunk_size,
        file_size,
        total_chunks,
    };

    {
        let mut app_state = state.lock().await;
        app_state.metadata = Some(task_metadata);
    }

    ServerResponse::SetupSuccess(SetupResponse {
        file_name,
        file_size,
        total_chunks,
        xxh3,
    })
}

pub async fn start(
    resume_from_chunk_id: u64,
    state: SharedState,
    sender: mpsc::Sender<Message>,
    mut shutdown_rx: watch::Receiver<()>,
) {
    let result: Result<()> = async {
        let (file_path, chunk_size) = {
            let app_state = state.lock().await;
            let metadata = app_state
                .metadata
                .as_ref()
                .context("任务未初始化。请在 'start' 之前先调用 'setup'")?;
            (metadata.file_path.clone(), metadata.chunk_size)
        };

        // 生产者(加密) -> 消费者(上传)
        let (upload_tx, mut upload_rx) = mpsc::channel::<EncryptedChunk>(4);

        // 3) 启动上传任务（消费者）
        let sender_clone = sender.clone();
        let mut upload_shutdown_rx = shutdown_rx.clone();
        let upload_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // 一旦收到关闭信号，不等待队列清空，直接退出上传协程
                    _ = upload_shutdown_rx.changed() => {
                        info!("上传任务收到关闭信号，正在退出...");
                        break;
                    }
                    item = upload_rx.recv() => {
                        match item {
                            Some(encrypted_chunk) => {
                                let chunk_id = encrypted_chunk.id;
                                let passphrase_b64 = encrypted_chunk.passphrase_b64;
                                let data = encrypted_chunk.data;
                                let upload_start_time = std::time::Instant::now();
                                info!("开始上传分块 #{chunk_id}...");

                                let key = chunk_id.to_string();
                                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                                let put_fut = put_object(&key, &data);
                                let abortable_put = Abortable::new(put_fut, abort_reg);

                                // 竞争：上传完成 vs. 收到关闭信号 -> 中止上传
                                tokio::select! {
                                    res = abortable_put => {
                                        match res {
                                            Ok(put_res) => {
                                                if let Err(e) = put_res {
                                                    return Err(e.context(format!("分块 #{chunk_id} 上传失败 (已自动重试)")));
                                                }
                                                info!(
                                                    "分块 #{chunk_id} 上传成功, 耗时: {} s",
                                                    upload_start_time.elapsed().as_secs_f32()
                                                );
                                                let success_response = ServerResponse::ChunkReadyForDownload {
                                                    chunk_id, passphrase_b64, retry:false
                                                };
                                                if sender_clone.send(success_response.into()).await.is_err() {
                                                    error!("无法将 ChunkReadyForDownload 消息发送给客户端: 通道已关闭");
                                                    return Err(anyhow!("无法将 ChunkReadyForDownload 消息发送给客户端: 通道已关闭"));
                                                }
                                            }
                                            Err(_) => {
                                                // 被优雅关闭中止
                                                warn!("分块 #{chunk_id} 上传已被中止（优雅关闭）");
                                                break; // 退出上传协程
                                            }
                                        }
                                    }
                                    _ = upload_shutdown_rx.changed() => {
                                        info!("收到关闭信号，正在中止当前分块 #{chunk_id} 上传");
                                        abort_handle.abort(); // 使 abortable_put 立刻返回 Err(Aborted)
                                        break; // 退出上传协程
                                    }
                                }
                            }
                            None => {
                                // 队列关闭，正常退出
                                info!("上传队列已关闭，上传任务正在退出");
                                break;
                            }
                        }
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        // 4) 主任务：读取 + 加密（生产者）
        let encryption_result: Result<()> = async {
            let mut file = File::open(&file_path)
                .await
                .with_context(|| format!("无法打开文件: {}", file_path.display()))?;

            if resume_from_chunk_id > 0 {
                let offset = resume_from_chunk_id * chunk_size;
                file.seek(tokio::io::SeekFrom::Start(offset))
                    .await
                    .with_context(|| format!("无法跳转到文件偏移量 {offset}"))?;
            }

            let reader = BufReader::new(file);

            let mut file_reader_stream = Box::pin(
                stream::try_unfold(
                    (reader, resume_from_chunk_id),
                    move |(mut reader, chunk_id)| async move {
                        let mut buf = vec![0u8; chunk_size as usize];
                        let mut filled = 0;
                        loop {
                            if filled == chunk_size as usize { break; }
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
                            Ok(Some(((chunk_id, buf), (reader, chunk_id + 1))))
                        }
                    },
                ).map_err(anyhow::Error::from)
            );

            // 使用 select! 监听关闭信号，随时打断生产者
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        warn!("加密任务收到关闭信号，正在中断文件处理...");
                        return Err(anyhow!("任务被用户中断"));
                    }
                    item = file_reader_stream.try_next() => {
                        match item {
                            Ok(Some((chunk_id, data))) => {
                                let encrypt_start_time = std::time::Instant::now();
                                info!("开始加密分块 #{chunk_id}...");
                                let (encrypted_data, passphrase_b64) =
                                    encrypt_chunk(&data).with_context(|| format!("分块 #{chunk_id} 加密失败"))?;
                                info!(
                                    "分块 #{chunk_id} 加密成功，耗时: {} s，已发送到上传队列。",
                                    encrypt_start_time.elapsed().as_secs_f32()
                                );
                                let chunk_to_upload = EncryptedChunk { id: chunk_id, data: encrypted_data, passphrase_b64 };
                                if upload_tx.send(chunk_to_upload).await.is_err() {
                                    return Err(anyhow!("无法发送加密分块到上传队列：上传任务已终止"));
                                }
                            }
                            Ok(None) => {
                                info!("文件处理完成，所有分块已发送到上传队列");
                                break;
                            }
                            Err(e) => {
                                return Err(e);
                            }
                        }
                    }
                }
            }
            Ok(())
        }.await;

        // 5) 收尾：根据是否是“用户中断”决定如何处理上传协程
        let was_interrupted = matches!(&encryption_result, Err(e) if e.to_string() == "任务被用户中断");
        let encryption_successful = encryption_result.is_ok();

        if let Err(e) = encryption_result {
            if !was_interrupted {
                return Err(e);
            }
        }

        // 关闭生产者通道
        drop(upload_tx);

        if was_interrupted {
            // 优雅关闭：不等待上传把队列吃完，直接中止上传协程
            warn!("优雅关闭：不再等待上传队列清空，直接结束上传任务");
            upload_handle.abort();
            let _ = upload_handle.await; // 回收 JoinHandle，忽略结果
        } else {
            // 正常结束：等待上传处理完队列中剩余分块
            info!("等待上传任务处理完队列中剩余的分块...");
            upload_handle.await??;
        }

        // 根据任务完成情况发消息
        if was_interrupted {
            warn!("任务因用户中断而停止");
        } else if encryption_successful {
            info!("任务成功完成");
            let completed_response = ServerResponse::UploadCompleted;
            sender
                .send(completed_response.into())
                .await
                .context("无法发送 UploadCompleted 消息")?;
        }

        Ok(())
    }.await;

    // 统一错误出口（用户中断不视为错误）
    if let Err(e) = result {
        if e.to_string() == "任务被用户中断" {
            warn!("任务被用户中断，不发送错误消息给客户端");
        } else {
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
}

fn encrypt_chunk(input_data: &[u8]) -> anyhow::Result<(Vec<u8>, String)> {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);

    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = Key::<Aes256Gcm>::from_slice(&key);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, input_data)
        .map_err(|e| anyhow::anyhow!("加密失败: {:?}", e))?;

    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes[..]);
    result.extend_from_slice(&ciphertext);

    Ok((result, general_purpose::STANDARD.encode(key)))
}

pub async fn chunk_retry(chunk_id: u64, state: SharedState, sender: mpsc::Sender<Message>) {
    let result: Result<()> = async {
        info!("开始重试分块 #{chunk_id}...");
        let (file_path, chunk_size) = {
            let app_state = state.lock().await;
            let metadata = app_state
                .metadata
                .as_ref()
                .context("任务未初始化，无法处理单个分块")?;
            (metadata.file_path.clone(), metadata.chunk_size)
        };

        let mut file = File::open(&file_path)
            .await
            .with_context(|| format!("无法打开文件: {}", file_path.display()))?;

        let offset = chunk_id * chunk_size;
        file.seek(tokio::io::SeekFrom::Start(offset))
            .await
            .with_context(|| format!("无法跳转到文件偏移量 {offset} (分块 #{chunk_id})"))?;

        let mut chunk_data = Vec::with_capacity(chunk_size as usize);
        let bytes_read = file
            .take(chunk_size) // 限制最多读取 chunk_size 字节
            .read_to_end(&mut chunk_data)
            .await
            .with_context(|| format!("读取分块 #{chunk_id} 的数据时失败"))?;

        if bytes_read == 0 {
            return Err(anyhow!(
                "指定的块号 {} 超出文件范围，没有可读取的数据。",
                chunk_id
            ));
        }
        info!("成功读取分块 #{chunk_id}，大小: {} 字节", bytes_read);

        let encrypt_start_time = std::time::Instant::now();
        info!("开始加密分块 #{chunk_id}...");
        let (encrypted_data, passphrase_b64) =
            encrypt_chunk(&chunk_data).with_context(|| format!("分块 #{chunk_id} 加密失败"))?;
        info!(
            "分块 #{chunk_id} 加密成功，耗时: {} s",
            encrypt_start_time.elapsed().as_secs_f32()
        );

        let upload_start_time = std::time::Instant::now();
        info!("开始上传分块 #{chunk_id}...");
        put_object(&chunk_id.to_string(), &encrypted_data)
            .await
            .with_context(|| format!("分块 #{chunk_id} 上传失败 (已自动重试)"))?;
        info!(
            "分块 #{chunk_id} 上传成功, 耗时: {} s",
            upload_start_time.elapsed().as_secs_f32()
        );

        let success_response = ServerResponse::ChunkReadyForDownload {
            chunk_id,
            passphrase_b64,
            retry: true,
        };
        sender
            .send(success_response.into())
            .await
            .context("无法将 ChunkReadyForDownload 消息发送给客户端: 通道已关闭")?;

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
        let error_message = format!("处理分块 #{chunk_id} 失败:\n{error_chain}");

        error!("{}", error_message);

        let error_response = ServerResponse::Error(error_message);
        if sender.send(error_response.into()).await.is_err() {
            error!(
                "通道已关闭。无法为分块 #{chunk_id} 发送错误消息。最终错误是: {:?}",
                e
            );
        }
    }
}
