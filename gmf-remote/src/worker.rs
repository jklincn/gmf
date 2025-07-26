use crate::r2;
use crate::state::{AppState, ChunkProcessingStatus, ChunkState};
use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose;
use futures::stream::{self, StreamExt};
use gmf_common::NONCE_SIZE;
use gmf_common::TaskEvent;
use std::time::Instant;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};
use tracing::{error, info, instrument};

// --- 配置常量 ---
const CONCURRENCY: usize = 2;

pub struct ChunkJob {
    pub id: u32,
    pub data: Vec<u8>,
}

// --- 主任务调度器 ---
#[instrument(skip_all, name = "run_task")]
pub async fn run_task(state: AppState) -> anyhow::Result<()> {
    info!("Worker 任务已启动");

    // 1. 初始化
    let (metadata, sender, chunk_size) = {
        let context = state.0.lock().unwrap();
        let metadata = context
            .metadata
            .clone()
            .expect("Metadata should be set by setup");
        let sender = context
            .event_sender
            .clone()
            .expect("Sender should be set by start");
        let chunk_size = metadata.chunk_size;
        (metadata, sender, chunk_size)
    };

    let _ = sender.send(TaskEvent::ProcessingStart);
    info!("已发送 ProcessingStart 事件");

    let file = File::open(&metadata.source_path).await?;
    let reader = BufReader::new(file);
    let file_reader_stream = stream::unfold((reader, 1u32), |(mut reader, chunk_id)| async move {
        let mut buf = vec![0u8; chunk_size];
        let mut filled = 0;

        // read 不保证每次都读取 CHUNK_SIZE 字节，因此需要循环读取直到填满或 EOF
        loop {
            if filled == chunk_size {
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

    // --- 加密 ---
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    let passphrase_b64 = general_purpose::STANDARD.encode(key);
    let encrypted_data = match encrypt_chunk(&key, &job.data) {
        Ok(data) => data,
        Err(e) => {
            let reason = format!("加密失败: {e}");
            error!("{}", reason);
            return ChunkState {
                id: chunk_id,
                status: ChunkProcessingStatus::Failed { reason },
            };
        }
    };

    // --- 上传 ---
    if let Err(e) = upload_chunk(chunk_id, &encrypted_data).await {
        let reason = format!("上传失败: {e}");
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

    // --- 6. 成功完成 ---
    info!("分块处理完成");
    ChunkState {
        id: chunk_id,
        status: ChunkProcessingStatus::Completed,
    }
}

// --- 辅助函数 ---

async fn upload_chunk(chunk_id: u32, data: &[u8]) -> Result<()> {
    let start = Instant::now();
    r2::put_object(&chunk_id.to_string(), data).await?;
    let duration = start.elapsed();
    info!("上传耗时（秒）: {} s", duration.as_secs_f32());
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
