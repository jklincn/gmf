mod logging;
mod server;

use crate::logging::init_logging;
use crate::server::{AppState, SharedState};
use anyhow::Result;
use gmf_common::interface::{Message, ServerResponse};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use tracing::info;
#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_logging().expect("无法初始化文件日志系统");

    let state: SharedState = Arc::new(Mutex::new(AppState::default()));

    let (tx, mut rx) = mpsc::channel::<Message>(100);

    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if let Ok(json) = message.to_json() {
                println!("{}", json);
            }
        }
    });

    let ready = ServerResponse::Ready;
    tx.send(ready.into()).await?;

    info!("服务端启动成功，开始监听 stdin...");

    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line_buffer = String::new();

    loop {
        line_buffer.clear();
        match stdin.read_line(&mut line_buffer).await {
            Ok(0) => break, // 读到 0 字节表示 EOF，客户端已关闭输入，正常退出循环。
            Ok(_) => {
                let line = line_buffer.trim();
                if line.is_empty() {
                    continue;
                }
                match Message::from_json(line) {
                    Ok(message) => {
                        let _ = server::handle_message(message, state.clone(), tx.clone()).await;
                    }
                    Err(e) => {
                        let error_response =
                            ServerResponse::InvalidRequest(format!("无法解析的JSON: {}", e));

                        if tx.send(error_response.into()).await.is_err() {
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                let fatal_error_response =
                    ServerResponse::Error(format!("致命 I/O 错误，服务端将退出: {}", e));
                let _ = tx.send(fatal_error_response.into()).await;
                break;
            }
        }
    }

    drop(tx);

    writer_task.await?;

    Ok(())
}
