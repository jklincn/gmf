use crate::state::{AppState, ChunkState, ChunkStatus};
use anyhow::Result;
use futures::stream::{self, StreamExt};
use r2::TaskEvent;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, instrument, warn};

const CONCURRENCY: usize = 2;

pub async fn run_task(state: AppState, path: PathBuf) {
    info!("开始执行任务");
    let sender = {
        let context = state.0.lock().expect("Mutex should not be poisoned here");
        context.event_sender.clone()
    };

    let sender = sender.unwrap();

    // 给客户端发送开始执行
    let _ = sender.send(TaskEvent::ProcessingStart);

    info!("开始分块和加密");
    let manifest = match r2::split_and_encrypt(&path).await {
        Ok(m) => {
            info!(total_chunks = m.total_chunks, "分块和加密完成");
            m
        }
        Err(e) => {
            error!(error = %e, "分块或加密失败");
            let _ = sender.send(TaskEvent::Error {
                message: e.to_string(),
            });
            return;
        }
    };

    // 保存分块信息到上下文中
    {
        let mut context = state.0.lock().unwrap();
        for chunk_info in &manifest.chunks {
            context.chunks.insert(
                chunk_info.id,
                ChunkState {
                    info: chunk_info.clone(),
                    status: ChunkStatus::Pending,
                    remote_path: None,
                },
            );
        }
    }

    // 给客户端发送分块完成
    let _ = sender.send(TaskEvent::SplitComplete {
        manifest: manifest.clone(),
    });

    // 设置并发
    let chunk_ids_stream = stream::iter(manifest.chunks.into_iter().map(|c| c.id));

    // 使用 map 将每个任务转换成一个 future，然后用 buffer_unordered 并发执行
    let results: Vec<Result<()>> = chunk_ids_stream
        .map(|chunk_id| {
            let task_state = state.clone();
            process_single_chunk(task_state, chunk_id)
        })
        .buffer_unordered(CONCURRENCY) // <-- 以指定并发数执行这些 future
        .collect() // <-- 收集所有 future 的结果
        .await;

    // --- 关键的状态检查逻辑 ---
    let mut successful_chunks = 0;
    for result in results {
        if result.is_ok() {
            successful_chunks += 1;
        }
    }

    info!(
        total = manifest.total_chunks,
        successful = successful_chunks,
        "所有分块处理完毕"
    );

    // 最终检查
    if successful_chunks == manifest.total_chunks as usize {
        info!("任务成功结束");
        // 给客户端发送执行完成
        let _ = sender.send(TaskEvent::TaskCompleted);
    } else {
        let failed_count = manifest.total_chunks as usize - successful_chunks;
        error!("任务失败，有 {} 个分块未能成功处理", failed_count);
        let _ = sender.send(TaskEvent::Error {
            message: format!("任务处理失败，{} 个分块出错", failed_count),
        });
    }
}

// 辅助函数，模拟可能失败的上传
async fn upload_chunk_simulation(chunk_id: u32) -> Result<String> {
    sleep(Duration::from_secs(2)).await;
    Ok(format!("/remote/path/for/chunk/{}", chunk_id))
}

// 使用 #[instrument] 来追踪单个分块的处理过程
#[instrument(skip(state), fields(chunk_id = chunk_id))]
async fn process_single_chunk(state: AppState, chunk_id: u32) -> Result<()> {
    info!("开始处理分块");

    let sender = {
        let context = state.0.lock().expect("Mutex should not be poisoned here");
        context.event_sender.clone()
    };

    let sender = sender.unwrap();

    let mut ack_receiver = sender.subscribe();

    // todo: 上传
    info!("正在上传分块");
    let remote_path = match upload_chunk_simulation(chunk_id).await {
        Ok(path) => {
            info!("模拟上传完成");
            path
        }
        Err(e) => {
            error!(error = %e, "上传失败");
            if let Ok(mut context) = state.0.lock() {
                if let Some(chunk) = context.chunks.get_mut(&chunk_id) {
                    chunk.status = ChunkStatus::Failed("上传失败".to_string());
                }
            }
            return Err(e.context("上传失败"));
        }
    };

    // 更新状态
    if let Ok(mut context) = state.0.lock() {
        if let Some(chunk) = context.chunks.get_mut(&chunk_id) {
            chunk.status = ChunkStatus::ReadyForDownload;
            chunk.remote_path = Some(remote_path.clone());
        }
    }
    let _ = sender.send(TaskEvent::ChunkReadyForDownload {
        chunk_id,
        remote_path,
    });

    // 等待确认
    info!("等待客户端确认");
    match tokio::time::timeout(Duration::from_secs(60), async {
        // <-- 加入60秒超时
        loop {
            match ack_receiver.recv().await {
                Ok(TaskEvent::ChunkAcknowledged { chunk_id: ack_id }) if ack_id == chunk_id => {
                    info!("收到确认");
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = ?e, "等待确认时接收事件失败");
                    // 这里可以认为是一个可恢复的错误，或者直接返回错误
                    return Err(anyhow::anyhow!("接收事件通道关闭"));
                }
            }
        }
        Ok(())
    })
    .await
    {
        Ok(Ok(_)) => { /* 成功 */ }
        Ok(Err(e)) => return Err(e), // ack_receiver 内部错误
        Err(_) => {
            // 超时错误
            error!("等待客户端确认超时");
            if let Ok(mut context) = state.0.lock() {
                if let Some(chunk) = context.chunks.get_mut(&chunk_id) {
                    chunk.status = ChunkStatus::Failed("等待客户端确认超时".to_string());
                }
            }
            return Err(anyhow::anyhow!("等待客户端确认超时"));
        }
    }

    // 删除
    let local_path = if let Ok(context) = state.0.lock() {
        context
            .chunks
            .get(&chunk_id)
            .map(|c| c.info.local_path.clone())
    } else {
        None
    };

    if let Some(path) = local_path {
        info!(path = ?path, "模拟删除本地和远程文件");
        // todo: 删除本地与远程
        sleep(Duration::from_secs(5)).await;
    } else {
        warn!("未能在状态中找到本地路径进行删除");
    }

    // 更新最终状态
    if let Ok(mut context) = state.0.lock() {
        if let Some(chunk) = context.chunks.get_mut(&chunk_id) {
            chunk.status = ChunkStatus::Completed;
        }
    }
    info!("分块处理完成");
    Ok(())
}
