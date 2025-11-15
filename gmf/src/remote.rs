use crate::file::{ChunkResult, GMFFile, GmfSession};
use crate::communicator::SSHCommunicator;
use crate::ssh;
use crate::ui;
use anyhow::{Context, Result, anyhow, bail};
use gmf_common::{interface::*, utils::format_size};
use std::collections::HashSet;
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

async fn check_remote(ssh: &mut ssh::SSHSession) -> Result<String> {
    const REMOTE_BIN_DIR: &str = "$HOME/.local/bin";
    const REMOTE_PATH: &str = "$HOME/.local/bin/gmf-remote";

    // 检查远程程序 SHA256 的命令
    let check_sha_cmd = format!(r#"sha256sum "{REMOTE_PATH}" 2>/dev/null | cut -d' ' -f1"#);

    let needs_install = match ssh.exec_once(&check_sha_cmd).await {
        Ok(out) if out.exit_code == 0 => {
            // 检查 SHA256 是否匹配
            out.output.trim() != GMF_REMOTE_SHA256
        }
        // 任何错误（如命令不存在）都视为需要安装
        _ => true,
    };

    // 如果需要，则进行安装/更新
    if needs_install {
        ui::log_info("正在安装服务端至远程目录 (~/.local/bin/gmf-remote)...");
        ssh.untar_from_memory(GMF_REMOTE_TAR_GZ, REMOTE_BIN_DIR)
            .await?;
    }

    Ok(REMOTE_PATH.to_string())
}

pub struct GMFClient {
    // 用于向 SSHCommunicator 发送命令
    command_tx: mpsc::Sender<ClientRequest>,
    // 用于从 SSHCommunicator 接收响应
    response_rx: mpsc::Receiver<ServerResponse>,
    // SSHCommunicator 任务的句柄，用于在 shutdown 时等待它结束
    comm_handle: JoinHandle<()>,
    // 持有底层的 SSH 会话对象，以便能够关闭它
    ssh_session: ssh::SSHSession,
    // GMF 文件管理
    file: Option<Arc<GmfSession>>,
    resume_from_chunk_id: u64,
}

impl GMFClient {
    /// 启动远程程序并创建一个新的交互式会话。
    pub async fn new(verbose: bool) -> Result<Self> {
        // 检查远程环境并获取 SSH 会话和远程程序路径
        let cfg = crate::config::get_config();
        let mut ssh_session = ssh::SSHSession::connect(cfg).await?;
        let remote_path = check_remote(&mut ssh_session).await?;

        ui::log_debug("正在启动 gmf-remote...");

        // 注入环境变量
        let command = format!(
            "ENDPOINT='{}' ACCESS_KEY_ID='{}' SECRET_ACCESS_KEY='{}' LOG='{}' {}",
            cfg.endpoint,
            cfg.access_key_id,
            cfg.secret_access_key,
            if verbose { "INFO" } else { "None" },
            remote_path
        );

        // 以交互模式调用远程程序
        let ssh_channel = ssh_session
            .exec_interactive(&command)
            .await
            .context("以交互模式启动 gmf-remote 失败")?;
        ui::log_debug("gmf-remote 已成功启动");

        let (command_tx, command_rx) = mpsc::channel(100);
        let (response_tx, response_rx) = mpsc::channel(100);

        let comm = SSHCommunicator::new(ssh_channel, command_rx, response_tx);
        let comm_handle = tokio::spawn(comm.run());

        ui::log_debug("本地通信器已成功启动");

        Ok(Self {
            command_tx,
            response_rx,
            comm_handle,
            ssh_session,
            file: None,
            resume_from_chunk_id: 0,
        })
    }

    pub async fn setup(&mut self, file_path: &str, chunk_size: u64) -> Result<()> {
        // 先接收一个 Ready 响应，表示远程程序已准备就绪
        match self.next_response().await? {
            Some(ServerResponse::Ready) => {
                ui::log_debug("远程程序已准备就绪");
            }
            Some(other_response) => {
                return Err(anyhow!(
                    "协议错误：期望收到 Ready 响应，但收到了 {:?}",
                    other_response
                ));
            }
            None => {
                return Err(anyhow!("连接已关闭，未收到任何响应"));
            }
        }

        let spinner = crate::ui::Spinner::new("正在取得文件信息...");
        self.send_request(ClientRequest::Setup {
            path: file_path.to_string(),
            chunk_size,
        })
        .await?;

        match self.next_response().await {
            Ok(Some(ServerResponse::SetupSuccess {
                file_name,
                file_size,
                total_chunks,
            })) => {
                let success_msg = format!(
                    "✅ 文件名称: {} (大小: {})",
                    file_name,
                    format_size(file_size)
                );
                spinner.finish(&success_msg);

                let (gmf_file, completed_chunks) =
                    GMFFile::new_or_resume(&file_name, file_size, total_chunks)?;

                ui::init_global_download_bar(total_chunks, completed_chunks)?;

                self.resume_from_chunk_id = completed_chunks;
                self.file = Some(Arc::new(GmfSession::new(gmf_file, completed_chunks)));
            }
            Ok(Some(ServerResponse::Error(msg))) => {
                let error_msg = format!("服务端错误: {msg}");
                spinner.abandon();
                return Err(anyhow!(error_msg));
            }
            Ok(Some(other_response)) => {
                let error_msg = format!("意外的响应: 收到了非预期的服务器响应 {other_response:?}");
                spinner.abandon();
                return Err(anyhow!(error_msg));
            }
            Ok(None) => {
                let error_msg = "连接中断: 在等待设置响应时连接已关闭";
                spinner.abandon();
                return Err(anyhow!(error_msg));
            }
            Err(e) => {
                // 这是 next_response() 本身发生的错误，如网络层或反序列化错误
                let error_msg = format!("通信错误: {e}");
                spinner.abandon();
                return Err(e.context(error_msg));
            }
        }

        Ok(())
    }

    pub async fn start(&mut self) -> Result<()> {
        let session = self
            .file
            .take()
            .ok_or_else(|| anyhow!("内部状态错误: file 未初始化"))?;

        let client_request = ClientRequest::Start {
            resume_from_chunk_id: self.resume_from_chunk_id,
        };

        ui::log_debug("正在发送 Start 请求...");
        self.send_request(client_request).await?;
        let remaining_size = match self.next_response().await? {
            Some(ServerResponse::StartSuccess { remaining_size }) => {
                ui::log_debug(&format!(
                    "服务端确认，需要传输的大小为 {} ，开始接收分块信息...",
                    format_size(remaining_size)
                ));
                remaining_size
            }
            Some(other_response) => bail!(
                "协议错误: 期望收到 StartSuccess，但收到 {:?}",
                other_response
            ),
            None => bail!("连接中断: 在等待 StartSuccess 响应时连接已关闭"),
        };

        // 主事件循环
        let loop_result = self.event_loop(session.clone()).await;

        match loop_result {
            Ok(upload_time) => {
                ui::log_debug("等待本地写入完成...");

                let session_to_finish = Arc::try_unwrap(session).map_err(|arc| {
                    anyhow!(
                        "无法获得 GmfSession 的唯一所有权，还有 {} 个引用存在。这可能是个逻辑错误。",
                        Arc::strong_count(&arc)
                    )
                })?;

                session_to_finish.wait_for_completion().await?;
                let secs = upload_time.as_secs_f64();
                let speed_str = if secs > 0.0 {
                    format!(
                        "{:.2} MB/s",
                        (remaining_size as f64 / secs) / (1024.0 * 1024.0)
                    )
                } else {
                    "N/A".to_string()
                };
                println!("⚡ 下载完成，平均传输速度: {speed_str}");
                Ok(())
            }
            Err(e) => {
                ui::abandon_download();
                Err(e)
            }
        }
    }

    /// 接收服务端消息的主循环
    async fn event_loop(&mut self, gmf_session: Arc<GmfSession>) -> Result<Duration> {
        let total_chunks = gmf_session.total_chunks;
        // 使用 Hashset 可以自动去重
        let mut completed_chunk_ids: HashSet<u64> = (0..self.resume_from_chunk_id).collect();
        let mut join_set: JoinSet<ChunkResult> = JoinSet::new();
        let mut upload_completed = false;
        let mut upload_time = Duration::ZERO;
        let start_time = std::time::Instant::now();
        ui::start_tick();

        ui::log_debug(&format!(
            "事件循环开始，总分块数: {}, 已完成分块数: {}, 需要下载分块数: {}",
            total_chunks,
            completed_chunk_ids.len(),
            total_chunks - completed_chunk_ids.len() as u64
        ));

        loop {
            // 检查是否所有分块都已完成
            if completed_chunk_ids.len() as u64 == total_chunks {
                // 等待所有剩余的任务完成
                while let Some(_) = join_set.join_next().await {}
                ui::log_debug("所有分块均已成功处理！");
                break;
            }

            tokio::select! {
                biased;

                // 处理已完成的分块任务
                Some(res) = join_set.join_next() => {
                    match res {
                        Ok(ChunkResult::Success(chunk_id)) => {
                            completed_chunk_ids.insert(chunk_id);
                            ui::log_debug(&format!("分块 #{chunk_id} 处理成功"));
                        },
                        Ok(ChunkResult::Timeout(chunk_id)) => {
                            ui::log_debug(&format!("分块 #{chunk_id} 下载超时，正在重试..."));
                            self.send_request(ClientRequest::Retry { chunk_id }).await?;
                        },
                        Ok(ChunkResult::Failure(chunk_id, err)) => {
                            let error_msg = format!("分块 #{chunk_id} 处理失败: {err:?}");
                            bail!(error_msg);
                        },
                        Err(join_err) => {
                            let error_msg = format!("任务执行出现严重错误 (panic): {join_err:?}");
                            bail!(error_msg);
                        },
                    }
                },

                // 接收服务端响应（上传超时 30 秒）
                resp = tokio::time::timeout(Duration::from_secs(30), self.response_rx.recv()), if !upload_completed => {
                    match resp {
                        Ok(Some(response)) => {
                            match response {
                                ServerResponse::ChunkReadyForDownload { chunk_id, passphrase_b64, retry } => {
                                    let session_clone = gmf_session.clone();
                                    join_set.spawn(async move {
                                        session_clone.handle_chunk(chunk_id, passphrase_b64, retry).await
                                    });
                                },
                                ServerResponse::UploadCompleted => {
                                    upload_time = start_time.elapsed();
                                    upload_completed = true;
                                    ui::log_debug("服务端上传已完成");
                                },
                                ServerResponse::Error(msg) => {
                                    let error_msg = format!("收到服务端错误: {msg}");
                                    bail!(error_msg);
                                },
                                other => {
                                    ui::log_warn(&format!("收到非关键消息: {other:?}"));
                                }
                            }
                        },
                        Ok(None) => {
                            bail!("连接中断: 服务端在任务完成前关闭了连接");
                        }
                        Err(_) => {
                            bail!("等待远程服务端响应超时，绝大概率为首次上传超时，请重试");
                        }
                    }
                },

                _ = tokio::time::sleep(Duration::from_millis(100)), if upload_completed && !join_set.is_empty() => {},
            }
        }

        Ok(upload_time)
    }

    /// 向远程程序发送一条指令
    pub async fn send_request(&self, request: ClientRequest) -> Result<()> {
        self.command_tx
            .send(request)
            .await
            .context("向 I/O Actor 发送命令失败，通道可能已关闭")
    }

    /// 接收下一个响应，带有超时机制
    pub async fn next_response(&mut self) -> Result<Option<ServerResponse>> {
        match tokio::time::timeout(Duration::from_secs(10), self.response_rx.recv()).await {
            Ok(Some(response)) => Ok(Some(response)),
            Ok(None) => Ok(None),
            Err(_) => Err(anyhow!("等待响应超时")),
        }
    }

    /// 发送退出指令，并等待程序结束
    pub async fn shutdown(mut self) -> Result<()> {
        // 关闭命令发送通道，这将导致 SSHCommunicator 退出
        drop(self.command_tx);

        // 等待 SSHCommunicator 任务结束
        match tokio::time::timeout(Duration::from_secs(10), self.comm_handle).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => ui::log_error(&format!("等待 I/O Actor 退出时发生错误: {e:?}")),
            Err(_) => ui::log_warn("等待 I/O Actor 退出超时！"),
        }

        // 关闭 SSH 会话
        self.ssh_session.close().await?;

        Ok(())
    }
}
