use anyhow::{Context, Result};
use gmf_common::interface::{ClientRequest, Message, ServerResponse};
use russh::{Channel, ChannelMsg, client::Msg as ClientMsg};
use tokio::sync::mpsc;

use crate::ui;

/// SSHCommunicator 负责所有底层的 SSH I/O 操作。
pub struct SSHCommunicator {
    // SSH 通道，这是它独占的资源
    ssh_channel: Channel<ClientMsg>,
    // 用于接收上层（Session）发来的待发送命令
    command_rx: mpsc::Receiver<ClientRequest>,
    // 用于向上层（Session）发送从网络接收到的响应
    response_tx: mpsc::Sender<ServerResponse>,
}

impl SSHCommunicator {
    /// 创建一个新的 SSHCommunicator 实例。
    pub fn new(
        ssh_channel: Channel<ClientMsg>,
        command_rx: mpsc::Receiver<ClientRequest>,
        response_tx: mpsc::Sender<ServerResponse>,
    ) -> Self {
        Self {
            ssh_channel,
            command_rx,
            response_tx,
        }
    }

    /// 启动 Actor 的主事件循环。
    /// 这个方法会一直运行，直到发生错误或通道关闭。
    pub async fn run(mut self) {
        let mut buffer = Vec::new(); // 用于缓冲不完整的行
        let mut command_channel_closed = false;

        loop {
            tokio::select! {
                cmd_result = self.command_rx.recv(), if !command_channel_closed => {
                    match cmd_result {
                        Some(request) => {
                            if self.handle_request(request).await.is_err() {
                                ui::log_error("发送命令失败, I/O Actor 退出。");
                                break;
                            }
                        }
                        None => {
                            command_channel_closed = true;
                            if let Err(e) = self.ssh_channel.eof().await {
                                ui::log_error(&format!("发送 EOF 失败: {e:?}"));
                                break;
                            }
                        }
                    }
                }

                maybe_msg = self.ssh_channel.wait() => {
                    match maybe_msg {
                        Some(msg) => {
                            if !self.handle_response(msg, &mut buffer).await {
                                break;
                            }
                        }
                        None => {
                            // 通道被远端关闭
                            break;
                        }
                    }
                }
            }
        }
    }

    /// 处理一个待发送的 ClientRequest。
    async fn handle_request(&mut self, request: ClientRequest) -> Result<()> {
        let message: Message = request.into();
        let json_string = message.to_json().context("序列化请求失败")?;
        let data_to_send = format!("{json_string}\n");
        self.ssh_channel.data(data_to_send.as_bytes()).await?;
        Ok(())
    }

    /// 处理一个从 SSH 通道接收到的 ChannelMsg。
    /// 返回 `true` 表示继续循环，`false` 表示应该退出。
    async fn handle_response(&mut self, msg: ChannelMsg, buffer: &mut Vec<u8>) -> bool {
        match msg {
            ChannelMsg::Data { data } => {
                buffer.extend_from_slice(&data);
                // 循环处理缓冲区中所有完整的行
                while let Some(newline_pos) = buffer.iter().position(|&b| b == b'\n') {
                    let line_bytes = buffer.drain(..=newline_pos).collect::<Vec<u8>>();
                    let decoded_line = String::from_utf8_lossy(&line_bytes);
                    let line_str = decoded_line.trim();

                    if line_str.is_empty() {
                        continue;
                    }

                    match Message::from_json(line_str) {
                        Ok(Message::Response(response)) => {
                            if self.response_tx.send(response).await.is_err() {
                                // 上层已关闭，我们也应该退出
                                return false;
                            }
                        }
                        _ => ui::log_error(&format!("收到无效或非响应类型的消息: {line_str}")),
                    }
                }
            }
            ChannelMsg::ExitStatus { exit_status: _ } => {
                return false; // 退出循环
            }
            _ => {} // 忽略其他消息
        }
        true // 继续循环
    }
}
