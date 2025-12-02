mod logging;
mod metadata;
mod msg;
mod server;

use crate::logging::init_logging;
use anyhow::Result;
use gmf_common::interface::{ClientRequest, Message, ServerResponse, StartResponse};
use gmf_common::r2;
use std::time::Duration;
use tokio::{sync::mpsc, task::JoinHandle, time::interval};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

fn start_heartbeat_task(tx: mpsc::Sender<Message>, shutdown_token: CancellationToken) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(1));

        // 启动后先立即发一次心跳
        if tx.send(ServerResponse::Heartbeat.into()).await.is_err() {
            error!("无法发送初始心跳，通信通道已关闭");
            return;
        }

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if tx.send(ServerResponse::Heartbeat.into()).await.is_err() {
                        error!("发送心跳失败，通信通道已关闭，心跳任务退出");
                        break;
                    }
                }
                _ = shutdown_token.cancelled() => {
                    info!("收到关闭信号，心跳任务退出");
                    break;
                }
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_logging().expect("无法初始化文件日志系统");

    // 创建本地通信器
    let (tx, mut rx) = mpsc::channel::<Message>(100);
    let writer_to_client = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg.to_json() {
                Ok(json) => println!("{json}"),
                Err(e) => eprintln!("writer_to_client: JSON 序列化失败: {e}"),
            }
        }
    });

    info!("writer_to_client 已启动");

    // 创建用于优雅关闭的 watch channel
    let shutdown_token = CancellationToken::new();

    if let Err(e) = r2::init(None).await {
        let _ = tx
            .send(ServerResponse::Error(format!("远程 R2 客户端初始化失败: {e}")).into())
            .await;
        error!("远程 R2 客户端初始化失败: {e}");
        return Ok(());
    }

    // 开始监听 stdin
    let mut request_rx = msg::start_stdin_request_channel(tx.clone());

    // 开启心跳
    start_heartbeat_task(tx.clone(), shutdown_token.clone());

    info!("服务端启动成功!");

    // 主循环,监听来自客户端的消息
    let mut start_handle: Option<JoinHandle<()>> = None;
    while let Some(request) = request_rx.recv().await {
        match request {
            ClientRequest::Setup { path, chunk_size } => {
                let response = server::setup(path, chunk_size).await;
                if tx.send(response.into()).await.is_err() {
                    error!("Channel closed. Could not send setup response.");
                }
            }
            ClientRequest::Start { mut chunk_bitmap } => {
                let metadata = metadata::global_metadata();

                // 修复 Bitmap 长度：因为在分配时至少用一个完整的u64，反序列化恢复了所有的存储位
                let total_chunks = metadata.total_chunks() as usize;
                if chunk_bitmap.bits.len() > total_chunks {
                    chunk_bitmap.bits.truncate(total_chunks);
                }
                metadata::init_chunk_bitmap(chunk_bitmap.clone());
                let chunk_size = metadata.chunk_size();
                let file_size = metadata.file_size();
                let sent_bytes = chunk_bitmap.completed_count() * chunk_size;
                let remaining_size = file_size.saturating_sub(sent_bytes);
                let ack = ServerResponse::StartSuccess(StartResponse { remaining_size });
                if tx.send(ack.into()).await.is_err() {
                    error!("Channel closed. Could not send StartSuccess ack.");
                }
                let handle = tokio::spawn(server::start(tx.clone(), shutdown_token.clone()));
                start_handle = Some(handle);
            }
        }
    }

    info!("正在通知所有任务关闭...");

    shutdown_token.cancel();

    if let Some(handle) = start_handle {
        info!("等待 start 任务完成清理...");
        let abort_handle = handle.abort_handle();
        match tokio::time::timeout(Duration::from_secs(10), handle).await {
            Ok(Ok(())) => info!("start 任务已成功关闭"),
            Ok(Err(e)) if e.is_panic() => error!("start 任务在关闭过程中 panic: {e:?}"),
            Ok(Err(_)) => info!("start 任务已被中止"),
            Err(_) => {
                error!("等待 start 任务超时, 将强制中止");
                abort_handle.abort();
            }
        }
    }

    // 通知 writer_to_client 退出
    drop(tx);
    if let Err(e) = writer_to_client.await {
        error!("等待 writer_to_client 退出时发生错误: {e:?}");
    } else {
        info!("writer_to_client 已成功退出");
    }

    info!("服务端已成功关闭。");
    Ok(())
}
