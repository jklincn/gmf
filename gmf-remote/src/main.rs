mod logging;
mod server;

use crate::logging::init_logging;
use crate::server::{AppState, SharedState};
use anyhow::Result;
use gmf_common::interface::{Message, ServerResponse};
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{Mutex, mpsc, watch},
    task::JoinHandle,
};
use tracing::{error, info};

fn start_heartbeat_task(tx: mpsc::Sender<Message>, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        use std::time::Duration;
        use tokio::time::interval;

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
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() {
                        info!("收到关闭信号，心跳任务退出");
                    }
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
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg.to_json() {
                Ok(json) => println!("{json}"),
                Err(e) => eprintln!("writer_task: JSON 序列化失败: {e}"),
            }
        }
    });

    info!("writer_task 已启动");

    // 初始化
    let state: SharedState = Arc::new(Mutex::new(AppState::default()));

    // 创建用于优雅关闭的 watch channel
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    start_heartbeat_task(tx.clone(), shutdown_rx.clone());

    info!("服务端启动成功, 开始监听 stdin...");

    // 主循环,监听来自客户端的消息
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line_buffer = String::new();
    let mut task_handle: Option<JoinHandle<()>> = None;
    loop {
        line_buffer.clear();
        match stdin.read_line(&mut line_buffer).await {
            Ok(0) => {
                info!("stdin EOF: 客户端已关闭，会话即将结束");
                break;
            }
            Ok(_) => {
                let line = line_buffer.trim();
                if line.is_empty() {
                    continue;
                }
                let msg = match Message::from_json(line) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = tx
                            .send(ServerResponse::Error(format!("无法解析 JSON: {e}")).into())
                            .await;
                        continue;
                    }
                };
                // 业务处理
                match server::handle_message(msg, state.clone(), tx.clone(), shutdown_rx.clone())
                    .await
                {
                    Ok(Some(handle)) => {
                        if let Some(old) = task_handle.take() {
                            old.abort();
                        }
                        task_handle = Some(handle);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // 发送业务错误
                        let _ = tx
                            .send(ServerResponse::Error(format!("处理消息失败: {e}")).into())
                            .await;
                        // 不 break，一样继续监听
                    }
                }
            }
            Err(e) => {
                error!("读取 stdin 时发生错误: {}", e);
                let _ = tx
                    .send(ServerResponse::Error(format!("读取 stdin 失败: {e}")).into())
                    .await;
                break;
            }
        }
    }

    info!("正在通知所有任务关闭...");

    let _ = shutdown_tx.send(());

    if let Some(handle) = task_handle {
        info!("等待任务完成清理...");
        let abort_handle = handle.abort_handle();
        match tokio::time::timeout(Duration::from_secs(10), handle).await {
            Ok(result) => match result {
                Ok(_) => info!("任务已成功关闭。"),
                Err(e) if e.is_panic() => error!("任务在关闭过程中发生 panic"),
                Err(_) => info!("任务被成功中止"),
            },
            Err(_) => {
                error!("等待任务超时, 将强制中止。");
                abort_handle.abort();
            }
        }
    }

    // 通知 writer_task 退出
    drop(tx);
    if let Err(e) = writer_task.await {
        error!("等待 writer_task 退出时发生错误: {e:?}");
    } else {
        info!("writer_task 已成功退出");
    }

    info!("服务端已成功关闭。");
    Ok(())
}
