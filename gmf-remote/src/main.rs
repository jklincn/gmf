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
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_logging().expect("无法初始化文件日志系统");

    let state: SharedState = Arc::new(Mutex::new(AppState::default()));
    let (tx, mut rx) = mpsc::channel::<Message>(100);

    // 创建用于优雅关闭的 watch channel
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let mut task_handle: Option<JoinHandle<()>> = None;

    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if let Ok(json) = message.to_json() {
                println!("{json}");
            }
        }
        info!("消息输出任务已退出");
    });

    // 发送 Ready 消息,通知客户端可以开始通信
    let ready = ServerResponse::Ready;
    tx.send(ready.into()).await?;

    info!("服务端启动成功,开始监听 stdin...");

    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line_buffer = String::new();

    // 主循环,监听来自客户端的消息
    loop {
        line_buffer.clear();
        match stdin.read_line(&mut line_buffer).await {
            Ok(0) => {
                info!("收到 EOF,客户端已断开连接。开始优雅关闭...");
                break; // 客户端关闭输入,正常退出循环
            }
            Ok(_) => {
                let line = line_buffer.trim();
                if line.is_empty() {
                    continue;
                }
                match Message::from_json(line) {
                    Ok(message) => {
                        match server::handle_message(
                            message,
                            state.clone(),
                            tx.clone(),
                            shutdown_rx.clone(),
                        )
                        .await
                        {
                            Ok(Some(handle)) => {
                                if let Some(old_handle) = task_handle.take() {
                                    warn!("新任务启动，正在中止旧任务...");
                                    old_handle.abort();
                                }
                                task_handle = Some(handle);
                            }
                            Ok(None) => {}
                            Err(e) => {
                                error!("处理消息时发生错误: {}", e);
                                let error_response =
                                    ServerResponse::Error(format!("消息处理失败: {e}"));
                                if tx.send(error_response.into()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let error_response = ServerResponse::Error(format!("无法解析的JSON: {e}"));
                        if tx.send(error_response.into()).await.is_err() {
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!("读取 stdin 时发生致命错误: {}", e);
                let fatal_error_response =
                    ServerResponse::Error(format!("致命 I/O 错误,服务端将退出: {e}"));
                let _ = tx.send(fatal_error_response.into()).await;
                break;
            }
        }
    }

    info!("正在通知所有任务关闭...");

    if shutdown_tx.send(()).is_err() {
        info!("没有活动的任务需要通知关闭。");
    }

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
                error!("等待任务超时!将强制中止。");
                // 如果优雅关闭失败,最后的手段是强制中止
                abort_handle.abort();
            }
        }
    }

    // 关闭主消息通道,这将让 writer_task 退出
    drop(tx);

    // 等待 writer_task 完成
    match tokio::time::timeout(Duration::from_secs(3), writer_task).await {
        Ok(result) => {
            if let Err(e) = result {
                error!("消息输出任务退出时发生错误: {:?}", e);
            }
        }
        Err(_) => {
            warn!("等待消息输出任务超时，强制继续关闭");
        }
    }

    info!("服务端已成功关闭。");
    Ok(())
}
