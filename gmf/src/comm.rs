use anyhow::{Context, Result};
use gmf_common::interface::{ClientRequest, Message, ServerResponse};
use russh::{Channel, ChannelMsg, client::Msg as ClientMsg};
use tokio::sync::mpsc;

use crate::ui;

/// 负责底层通信
pub struct SSHCommunicator {
    ssh_channel: Channel<ClientMsg>,
    command_rx: mpsc::Receiver<ClientRequest>,
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

    /// 处理一个待发送的 ClientRequest。
    async fn handle_request(&mut self, request: ClientRequest) -> Result<()> {
        let message: Message = request.into();
        let json_string = message.to_json().context("序列化请求失败")?;
        let data_to_send = format!("{json_string}\n");
        self.ssh_channel.data(data_to_send.as_bytes()).await?;
        Ok(())
    }

    /// 处理一个从 SSH 通道接收到的 ChannelMsg。
    async fn handle_response(&mut self, msg: ChannelMsg, buffer: &mut Vec<u8>) {
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
                                return;
                            }
                        }
                        _ => ui::log_error(&format!("收到无效或非响应类型的消息: {line_str}")),
                    }
                }
            }
            _ => {
                ui::log_debug(&format!("收到非数据消息: {:?}", msg));
            }
        }
    }

    /// 启动 Comm 的主事件循环。
    pub async fn run(mut self) {
        let mut buffer = Vec::new();

        loop {
            tokio::select! {
                // 有新的客户端请求需要发送到远端
                cmd_result = self.command_rx.recv() => {
                    match cmd_result {
                        Some(request) => {
                            if self.handle_request(request).await.is_err() {
                                ui::log_error("发送命令失败，通信器退出。");
                                break;
                            }
                        }
                        None => {break;}
                    }
                }

                // 远端发回了新的消息
                maybe_msg = self.ssh_channel.wait() => {
                    match maybe_msg {
                        Some(msg) => {self.handle_response(msg, &mut buffer).await;}
                        None => {break;}
                    }
                }
            }
        }
    }
}
