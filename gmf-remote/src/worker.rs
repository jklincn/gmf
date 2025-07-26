use crate::r2;
use crate::state::{AppState, ChunkProcessingStatus, ChunkState, TaskEvent};
use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose;
use futures::stream::{self, StreamExt};
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::broadcast;
use tokio::time::sleep;
use tracing::{error, info, instrument, warn};

// --- 配置常量 ---
const CONCURRENCY: usize = 2;
const NONCE_SIZE: usize = 12;
pub const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10MB

// --- 作业定义 ---
pub struct ChunkJob {
    pub id: u32,
    pub data: Vec<u8>,
}

// --- 主任务调度器 ---
#[instrument(skip_all, name = "run_task")]
pub async fn run_task(state: AppState) -> anyhow::Result<()> {
    info!("Worker 任务已启动");

    // 1. 初始化
    let (metadata, sender) = {
        let context = state.0.lock().unwrap();
        let metadata = context
            .metadata
            .clone()
            .expect("Metadata should be set by setup");
        let sender = context
            .event_sender
            .clone()
            .expect("Sender should be set by start");
        (metadata, sender)
    };

    let _ = sender.send(TaskEvent::ProcessingStart);
    info!("已发送 ProcessingStart 事件");

    let expected = ((metadata.source_size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64) as usize;
    if expected != metadata.total_chunks as usize {
        warn!(
            expected = expected,
            meta_total = metadata.total_chunks,
            "metadata.total_chunks 与实际计算不一致，将以实际读取为准"
        );
    }

    let file = File::open(&metadata.source_path).await?;
    let reader = BufReader::new(file);
    let file_reader_stream = stream::unfold((reader, 1u32), |(mut reader, chunk_id)| async move {
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut filled = 0;

        // read 不保证每次都读取 CHUNK_SIZE 字节，因此需要循环读取直到填满或 EOF
        loop {
            if filled == CHUNK_SIZE {
                break;
            }
            match reader.read(&mut buf[filled..]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    error!("文件读取失败: {}", e);
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
    });

    // 3. 并发调度与执行，并收集所有结果
    let final_chunk_states: Vec<ChunkState> = file_reader_stream
        .map(|job| process_single_chunk(state.clone(), job))
        .buffered(CONCURRENCY)
        .collect()
        .await;

    // 4. 最终状态检查：基于收集到的结果
    let mut completed_count = 0;
    let mut failed_count = 0;
    for chunk_state in &final_chunk_states {
        match chunk_state.status {
            ChunkProcessingStatus::Completed => completed_count += 1,
            ChunkProcessingStatus::Failed { .. } => failed_count += 1,
        }
    }

    info!(
        total = metadata.total_chunks,
        completed = completed_count,
        failed = failed_count,
        "所有分块处理完毕"
    );

    if failed_count == 0 && completed_count == metadata.total_chunks as usize {
        info!("任务成功结束");
        let _ = sender.send(TaskEvent::TaskCompleted);
        Ok(())
    } else {
        let message = format!(
            "任务处理失败。总共 {} 个分块，成功 {} 个，失败 {} 个。",
            metadata.total_chunks, completed_count, failed_count
        );
        error!("{}", message);
        let _ = sender.send(TaskEvent::Error {
            message: message.clone(),
        });
        bail!(message);
        // 或者：bail!(message);
    }
}

// --- 单个分块处理器 (Worker) ---
// 函数签名现在返回一个 ChunkState
#[instrument(skip(state, job), fields(chunk_id = job.id))]
async fn process_single_chunk(state: AppState, job: ChunkJob) -> ChunkState {
    let chunk_id = job.id;
    info!("开始处理分块");

    let sender = {
        let context = state.0.lock().unwrap();
        context.event_sender.clone().expect("Sender must exist")
    };

    // --- 1. 加密 ---
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    let passphrase_b64 = general_purpose::STANDARD.encode(&key);
    let encrypted_data = match encrypt_chunk(&key, &job.data) {
        Ok(data) => data,
        Err(e) => {
            let reason = format!("加密失败: {}", e);
            error!("{}", reason);
            return ChunkState {
                id: chunk_id,
                status: ChunkProcessingStatus::Failed { reason },
            };
        }
    };

    // --- 2. 上传 ---
    if let Err(e) = upload_chunk(chunk_id, &encrypted_data).await {
        let reason = format!("上传失败: {}", e);
        error!("{}", reason);
        return ChunkState {
            id: chunk_id,
            status: ChunkProcessingStatus::Failed { reason },
        };
    }
    info!("上传完成");

    // --- 3. 发送就绪事件给客户端 ---
    let _ = sender.send(TaskEvent::ChunkReadyForDownload {
        chunk_id,
        passphrase_b64,
    });

    // // --- 4. 等待客户端确认 ---
    // if wait_for_acknowledgement(&sender, chunk_id).await.is_err() {
    //     let reason = "等待客户端确认超时".to_string();
    //     error!("{}", reason);
    //     return ChunkState {
    //         id: chunk_id,
    //         status: ChunkProcessingStatus::Failed { reason },
    //     };
    // }
    // info!("收到确认");

    // // --- 5. 删除远程对象 ---
    // if let Err(e) = delete_chunk(chunk_id).await {
    //     let reason = format!("删除远程对象失败: {}", e);
    //     error!("{}", reason);
    //     warn!("未能清理远程对象: {}", chunk_id);
    //     return ChunkState {
    //         id: chunk_id,
    //         status: ChunkProcessingStatus::Failed { reason },
    //     };
    // }
    // info!("远程对象删除成功");

    // --- 6. 成功完成 ---
    info!("分块处理完成");
    ChunkState {
        id: chunk_id,
        status: ChunkProcessingStatus::Completed,
    }
}

// --- 辅助函数 ---

// /// 等待特定分块的确认消息
// async fn wait_for_acknowledgement(
//     sender: &broadcast::Sender<TaskEvent>,
//     chunk_id: u32,
// ) -> Result<(), tokio::time::error::Elapsed> {
//     let mut ack_receiver = sender.subscribe();
//     tokio::time::timeout(Duration::from_secs(10), async {
//         loop {
//             // 忽略接收错误，只关心是否收到正确的 ack
//             if let Ok(TaskEvent::ChunkAcknowledged { chunk_id: ack_id }) = ack_receiver.recv().await
//             {
//                 if ack_id == chunk_id {
//                     break;
//                 }
//             }
//         }
//     })
//     .await
// }

async fn upload_chunk(chunk_id: u32, data: &[u8]) -> Result<()> {
    let start = Instant::now();
    r2::put_object(&chunk_id.to_string(), data).await?;
    let duration = start.elapsed();
    info!("上传耗时（秒）: {} s", duration.as_secs_f32());
    Ok(())
}

async fn delete_chunk(chunk_id: u32) -> Result<()> {
    r2::delete_object(&chunk_id.to_string()).await?;
    Ok(())
}

fn encrypt_chunk(key_bytes: &[u8; 32], input_data: &[u8]) -> anyhow::Result<Vec<u8>> {
    // 生成随机 nonce（每个文件都唯一）
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, input_data)
        .map_err(|e| anyhow::anyhow!("加密失败: {:?}", e))?;

    // 输出格式：nonce || ciphertext
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes[..]);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}
