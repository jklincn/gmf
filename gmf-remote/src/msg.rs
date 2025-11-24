use gmf_common::interface::{ClientRequest, Message, ServerResponse};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
};
use tracing::{error, info};

/// 启动一个后台任务：
/// - 负责从 stdin 读取每一行
/// - 尝试解析为 Message
/// - 只把 Message::Request(...) 转成 ClientRequest 发到 tx
/// - 如果解析错误 / 协议错误，就通过 response_tx 发 Error 给客户端
///
/// 返回值：供业务层使用的 `mpsc::Receiver<ClientRequest>`
pub fn start_stdin_request_channel(
    response_tx: mpsc::Sender<Message>,
) -> mpsc::Receiver<ClientRequest> {
    let (req_tx, req_rx) = mpsc::channel::<ClientRequest>(100);

    // 启动一个后台任务读取 stdin
    tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin());
        let mut line_buffer = String::new();

        loop {
            line_buffer.clear();
            match stdin.read_line(&mut line_buffer).await {
                Ok(0) => {
                    info!("stdin EOF: 客户端已关闭，会话即将结束（stdin 读取任务退出）");
                    break;
                }
                Ok(_) => {
                    let line = line_buffer.trim();
                    if line.is_empty() {
                        continue;
                    }

                    // 1) 解析 JSON -> Message
                    let msg = match Message::from_json(line) {
                        Ok(m) => m,
                        Err(e) => {
                            // 解析失败，发 Error 给客户端，然后继续读下一行
                            let _ = response_tx
                                .send(ServerResponse::Error(format!("无法解析 JSON: {e}")).into())
                                .await;
                            continue;
                        }
                    };

                    // 2) 必须是 Request，否则协议错误
                    let request = match msg {
                        Message::Request(req) => req,
                        _ => {
                            let _ = response_tx
                                .send(
                                    ServerResponse::Error(
                                        "协议错误：收到了非 Request 类型的消息。".to_string(),
                                    )
                                    .into(),
                                )
                                .await;
                            continue;
                        }
                    };

                    // 3) 把合法的 ClientRequest 发到请求通道
                    if req_tx.send(request).await.is_err() {
                        info!("request 通道已关闭，stdin 读取任务退出");
                        break;
                    }
                }
                Err(e) => {
                    error!("读取 stdin 时发生错误: {}", e);
                    let _ = response_tx
                        .send(ServerResponse::Error(format!("读取 stdin 失败: {e}")).into())
                        .await;
                    break;
                }
            }
        }

        info!("stdin 读取任务结束");
    });

    req_rx
}
