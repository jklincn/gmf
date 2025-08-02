use crate::config::Config;
use crate::file::{ChunkResult, GMFFile, GmfSession};
use crate::io_actor::IoActor;
use crate::ssh::{self};
use crate::ui::{AllProgressBar, LogLevel, run_with_spinner};
use anyhow::{Context, Result, anyhow, bail};
use gmf_common::interface::*;
use gmf_common::utils::format_size;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

pub async fn check_remote(cfg: &Config) -> Result<(ssh::Session, String)> {
    let mut ssh = run_with_spinner(
        "正在连接远程服务器...",
        "✅ 远程服务器连接成功",
        ssh::Session::connect(cfg),
    )
    .await?;

    const REMOTE_BIN_DIR: &str = "$HOME/.local/bin";
    const REMOTE_PATH: &str = "$HOME/.local/bin/gmf-remote";

    // 检查远程程序 SHA256 的命令
    let check_sha_cmd = format!(
        r#"
        if [ -f "{REMOTE_PATH}" ]; then
            sha256sum "{REMOTE_PATH}" | cut -d' ' -f1
        else
            echo ""
        fi
    "#
    );

    let needs_install = match ssh.call_once(&check_sha_cmd).await {
        Ok((0, remote_sha)) => {
            // 检查 SHA256 是否匹配
            remote_sha.trim() != GMF_REMOTE_SHA256
        }
        _ => {
            // 任何错误（如命令不存在）都视为需要安装
            true
        }
    };

    // 如果需要，则进行安装/更新
    if needs_install {
        let msg = format!("正在安装服务端至远程目录 (~/.local/bin)...");
        run_with_spinner::<_, (), anyhow::Error>(&msg, "✅ 远程服务端安装成功", async {
            // 第一步：创建目录
            ssh.call_once(&format!("mkdir -p {REMOTE_BIN_DIR}")).await?;

            // 第二步：上传并解压
            ssh.untar_from_memory(GMF_REMOTE_TAR_GZ, REMOTE_BIN_DIR)
                .await?;

            Ok(())
        })
        .await?;
    }

    Ok((ssh, REMOTE_PATH.to_string()))
}

pub struct InteractiveSession {
    // 用于向 IoActor 发送命令
    command_tx: mpsc::Sender<ClientRequest>,
    // 用于从 IoActor 接收响应
    response_rx: mpsc::Receiver<ServerResponse>,
    // IoActor 任务的句柄，用于在 shutdown 时等待它结束
    actor_handle: JoinHandle<()>,
    // 持有底层的 SSH 会话对象，以便能够关闭它
    ssh_session: ssh::Session,
    // 进度条
    progress_bar: Option<Arc<AllProgressBar>>,
    // GMF 文件管理
    file: Option<Arc<GmfSession>>,
}

impl InteractiveSession {
    /// 启动远程程序并创建一个新的交互式会话。
    pub async fn new(config: &Config) -> Result<Self> {
        // 检查远程环境并获取 SSH 会话和远程程序路径
        let (mut ssh_session, remote_path) = check_remote(config).await?;

        info!("正在启动 gmf-remote...");
        let command = format!(
            "ENDPOINT='{}' ACCESS_KEY_ID='{}' SECRET_ACCESS_KEY='{}' {}",
            config.endpoint, config.access_key_id, config.secret_access_key, remote_path
        );
        // 以交互模式调用远程程序
        let ssh_channel = match ssh_session
            .call(&command, ssh::ExecutionMode::Interactive)
            .await?
        {
            ssh::CallResult::Interactive(channel) => {
                info!("gmf-remote 已成功启动");
                channel
            }
            ssh::CallResult::Once((code, out)) => {
                // 如果程序立即退出，说明启动失败，这是一个致命错误。
                return Err(anyhow!(
                    "尝试以交互模式启动 gmf-remote 失败，程序立即退出。退出码: {}, 输出: {}",
                    code,
                    out
                ));
            }
        };

        let (command_tx, command_rx) = mpsc::channel(100);
        let (response_tx, response_rx) = mpsc::channel(100);

        let actor = IoActor::new(ssh_channel, command_rx, response_tx);
        let actor_handle = tokio::spawn(actor.run());

        Ok(Self {
            command_tx,
            response_rx,
            actor_handle,
            ssh_session,
            progress_bar: None,
            file: None,
        })
    }

    pub async fn setup(
        &mut self,
        file_path: &str,
        chunk_size: u64,
        concurrency: u64,
    ) -> Result<()> {
        // 先接收一个 Ready 响应，表示远程程序已准备就绪
        match self.next_response().await? {
            Some(ServerResponse::Ready) => {
                info!("远程程序已准备就绪");
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

        let client_request = ClientRequest::Setup(SetupRequestPayload {
            path: file_path.to_string(),
            chunk_size,
            concurrency,
        });

        let spinner = crate::ui::Spinner::new("正在取得文件信息...");
        self.send_request(&client_request).await?;

        match self.next_response().await {
            Ok(Some(ServerResponse::SetupSuccess(setup_response))) => {
                let success_msg = format!(
                    "✅ 文件名称: {} (大小: {})",
                    setup_response.file_name,
                    format_size(setup_response.file_size)
                );

                spinner.finish(&success_msg);
                self.initialize_session(setup_response)?;
            }

            Ok(Some(ServerResponse::InvalidRequest(msg))) => {
                let error_msg = format!("❌ 请求无效: {}", msg);
                spinner.finish(&error_msg);
                return Err(anyhow!(error_msg));
            }
            Ok(Some(ServerResponse::NotFound(msg))) => {
                let error_msg = format!("❌ 找不到文件: {}", msg);
                spinner.finish(&error_msg);
                return Err(anyhow!(error_msg));
            }
            Ok(Some(ServerResponse::Error(msg))) => {
                let error_msg = format!("❌ 服务端错误: {}", msg);
                spinner.finish(&error_msg);
                return Err(anyhow!(error_msg));
            }

            // --- 发生了意外情况 ---
            Ok(Some(other_response)) => {
                let error_msg = format!(
                    "❌ 意外的响应: 收到了非预期的服务器响应 {:?}",
                    other_response
                );
                spinner.finish(&error_msg);
                return Err(anyhow!(error_msg));
            }
            Ok(None) => {
                let error_msg = "❌ 连接中断: 在等待设置响应时连接已关闭";
                spinner.finish(error_msg);
                return Err(anyhow!(error_msg));
            }
            Err(e) => {
                // 这是 next_response() 本身发生的错误，如网络层或反序列化错误
                let error_msg = format!("❌ 通信错误: {}", e);
                spinner.finish(&error_msg);
                return Err(e.context(error_msg));
            }
        }

        Ok(())
    }

    fn initialize_session(&mut self, setup_response: SetupResponse) -> Result<()> {
        let (gmf_file, completed_chunks) = GMFFile::new_or_resume(
            &setup_response.file_name,
            setup_response.file_size,
            setup_response.total_chunks,
        )?;

        let progress_bar = Arc::new(AllProgressBar::new(
            setup_response.total_chunks,
            completed_chunks,
            LogLevel::Info,
        )?);

        self.progress_bar = Some(progress_bar.clone());
        self.file = Some(Arc::new(GmfSession::new(
            gmf_file,
            completed_chunks,
            progress_bar,
        )));

        Ok(())
    }

    pub async fn start(&mut self) -> Result<()> {
        let progress_bar = self
            .progress_bar
            .clone()
            .ok_or_else(|| anyhow!("请先调用 setup() 方法以初始化客户端状态"))?;
        let session = self
            .file
            .take()
            .ok_or_else(|| anyhow!("内部状态错误: file 未初始化"))?;

        let resume_from_chunk_id = progress_bar.completed_chunks;
        let client_request = ClientRequest::Start(StartRequestPayload {
            resume_from_chunk_id,
        });

        info!("正在发送 Start 请求...");
        self.send_request(&client_request).await?;
        match self.next_response().await? {
            Some(ServerResponse::StartSuccess) => {
                info!("服务端确认，开始接收分块信息...");
            }
            Some(other_response) => bail!(
                "协议错误: 期望收到 StartSuccess，但收到 {:?}",
                other_response
            ),
            None => bail!("连接中断: 在等待 StartSuccess 响应时连接已关闭"),
        }

        // 主事件循环
        let loop_result = event_loop(&mut self.response_rx, session.clone(), &progress_bar).await;

        progress_bar.finish_download();
        
        info!("等待所有本地任务完成...");

        let session_to_finish = Arc::try_unwrap(session).map_err(|arc| {
            anyhow!(
                "无法获得 GmfSession 的唯一所有权，还有 {} 个引用存在。这可能是个逻辑错误。",
                Arc::strong_count(&arc)
            )
        })?;

        session_to_finish.wait_for_completion().await?;

        

        let _upload_time = match loop_result {
            Ok(time) => time,
            Err(e) => {
                return Err(e);
            }
        };

        warn!("\n所有任务完成，文件已在本地准备就绪");

        // warn!("\n所有任务完成，文件已在本地准备就绪。平均上传速度：{} MB/s", upload_time.as_millis());

        Ok(())
    }

    /// 向远程程序发送一条指令，并自动添加换行符。
    pub async fn send_request(&self, request: &ClientRequest) -> Result<()> {
        self.command_tx
            .send(request.clone())
            .await
            .context("向 I/O Actor 发送命令失败，通道可能已关闭")
    }

    pub async fn next_response(&mut self) -> Result<Option<ServerResponse>> {
        match tokio::time::timeout(Duration::from_secs(5), self.response_rx.recv()).await {
            Ok(Some(response)) => Ok(Some(response)),
            Ok(None) => Ok(None),
            Err(_) => Err(anyhow::anyhow!("等待响应超时",)),
        }
    }

    /// 发送退出指令，并等待程序结束。
    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.command_tx);

        match tokio::time::timeout(Duration::from_secs(5), self.actor_handle).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => error!("等待 I/O Actor 退出时发生错误: {e:?}"),
            Err(_) => warn!("等待 I/O Actor 退出超时！"),
        }

        self.ssh_session.close().await?;

        Ok(())
    }
}

async fn event_loop(
    response_rx: &mut mpsc::Receiver<ServerResponse>,
    session: Arc<GmfSession>,
    progress_bar: &Arc<AllProgressBar>,
) -> Result<Duration> {
    let mut join_set: JoinSet<ChunkResult> = JoinSet::new();
    let mut upload_completed = false;
    let mut upload_time = Duration::ZERO;
    let start_time = std::time::Instant::now();
    progress_bar.start_tick();

    loop {
        tokio::select! {
            biased;

            Some(res) = join_set.join_next() => {
                match res {
                    Ok(ChunkResult::Success(id)) => {
                        progress_bar.log_debug(&format!("分块 {id} 处理成功"));
                    },
                    Ok(ChunkResult::Failure(id, err)) => {
                        let error_msg = format!("分块 #{id} 本地处理失败: {err:?}");
                        progress_bar.log_error(&error_msg);
                        bail!(error_msg);
                    },
                    Err(join_err) => {
                        let error_msg = format!("任务执行出现严重错误 (panic): {join_err:?}");
                        progress_bar.log_error(&error_msg);
                        bail!(error_msg);
                    },
                }
            },

            resp = tokio::time::timeout(Duration::from_secs(15), response_rx.recv()), if !upload_completed => {
                match resp {
                    Ok(Some(response)) => {
                        match response {
                            ServerResponse::ChunkReadyForDownload { chunk_id, passphrase_b64 } => {
                                let session_clone = session.clone();
                                join_set.spawn(async move {
                                    session_clone.handle_chunk(chunk_id, passphrase_b64).await
                                });
                            },
                            ServerResponse::UploadCompleted => {
                                upload_time = start_time.elapsed();
                                upload_completed = true;
                            },
                            ServerResponse::Error(msg) => {
                                let error_msg = format!("收到服务端错误: {msg}");
                                progress_bar.log_error(&error_msg);
                                bail!(error_msg);
                            },
                            other => {
                                progress_bar.log_debug(&format!("收到非关键消息: {:?}", other));
                            }
                        }
                    },
                    Ok(None) => { // recv() 返回 None (通道关闭) 或 Err
                        bail!("连接中断: 服务端在任务完成前关闭了连接");
                    }
                    Err(_) => { // `timeout` 触发
                        bail!("等待响应超时");
                    }
                }
            },
        }

        if upload_completed && join_set.is_empty() {
            break;
        }
    }

    Ok(upload_time)
}
