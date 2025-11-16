use crate::args::GetArgs;
use crate::comm::SSHCommunicator;
use crate::file::{ChunkResult, DownloadSession};
use crate::ssh;
use crate::ui;
use anyhow::{Context, Result, anyhow, bail};
use gmf_common::{interface::*, utils::format_size};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
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
    args: GetArgs,
    // 用于向 SSHCommunicator 发送命令
    command_tx: mpsc::Sender<ClientRequest>,
    // 用于从 SSHCommunicator 接收响应
    response_rx: Option<mpsc::Receiver<ServerResponse>>,
    // 通过 handle 管理 comm 生命周期
    comm_handle: JoinHandle<()>,
    // 持有底层的 SSH 会话对象，以便能够关闭它
    ssh_session: ssh::SSHSession,

    resume_from_chunk_id: u64,

    // 等待 setup / start 结果的 oneshot sender
    ready_waiter: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    setup_waiter: Arc<Mutex<Option<oneshot::Sender<SetupResponse>>>>,
    start_waiter: Arc<Mutex<Option<oneshot::Sender<StartResponse>>>>,

    // dispatcher -> 下载循环 的事件通道
    download_session: Option<Arc<DownloadSession>>,
    download_tx: mpsc::Sender<DownloadEvent>,
    download_rx: mpsc::Receiver<DownloadEvent>,
}

impl GMFClient {
    /// 启动远程程序并创建一个新的交互式会话。
    pub async fn new(args: GetArgs) -> Result<Self> {
        // 检查远程环境并获取 SSH 会话和远程程序路径
        let cfg = crate::config::get_config();
        let mut ssh_session = ssh::SSHSession::connect(cfg)
            .await
            .context("连接远程服务器失败")?;
        let remote_path = check_remote(&mut ssh_session)
            .await
            .context("检查远程程序失败")?;

        ui::log_debug("正在启动 gmf-remote...");

        // 注入环境变量
        let command = format!(
            "ENDPOINT='{}' ACCESS_KEY_ID='{}' SECRET_ACCESS_KEY='{}' LOG='{}' {}",
            cfg.endpoint,
            cfg.access_key_id,
            cfg.secret_access_key,
            if args.verbose { "INFO" } else { "None" },
            remote_path
        );

        // 以交互模式调用远程程序
        let ssh_channel = ssh_session
            .exec_interactive(&command)
            .await
            .context("以交互模式启动 gmf-remote 失败")?;
        ui::log_debug("gmf-remote 已成功启动");

        // comm 持有发送命令的接收端以及响应的发送端，以及 ssh_channel
        // client 持有发送命令的发送端以及响应的接收端，以及 ssh_session
        let (command_tx, command_rx) = mpsc::channel(100);
        let (response_tx, response_rx) = mpsc::channel(100);

        let comm = SSHCommunicator::new(ssh_channel, command_rx, response_tx);
        let comm_handle = tokio::spawn(comm.run()); // 通过 handle 管理 comm 生命周期
        let (download_tx, download_rx) = mpsc::channel(100);

        ui::log_debug("本地通信器已成功启动");

        Ok(Self {
            args,
            command_tx,
            response_rx: Some(response_rx),
            comm_handle,
            ssh_session,
            resume_from_chunk_id: 0,
            ready_waiter: Arc::new(Mutex::new(None)),
            setup_waiter: Arc::new(Mutex::new(None)),
            start_waiter: Arc::new(Mutex::new(None)),
            download_session: None,
            download_tx,
            download_rx,
        })
    }

    async fn run_dispatcher(
        mut response_rx: mpsc::Receiver<ServerResponse>,
        download_tx: mpsc::Sender<DownloadEvent>,
        ready_waiter: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        setup_waiter: Arc<Mutex<Option<oneshot::Sender<SetupResponse>>>>,
        start_waiter: Arc<Mutex<Option<oneshot::Sender<StartResponse>>>>,
    ) -> Result<()> {
        use std::time::{Duration, Instant};

        let mut last_heartbeat = Instant::now();
        let timeout = Duration::from_secs(5);

        loop {
            tokio::select! {
                // 收到服务端消息
                maybe_resp = response_rx.recv() => {
                    match maybe_resp {
                        Some(resp) => {
                            match resp {
                                ServerResponse::Heartbeat => {
                                    last_heartbeat = Instant::now();
                                    ui::log_debug("收到心跳");

                                    if let Some(tx) = ready_waiter.lock().await.take() {
                                        let _ = tx.send(());
                                    }
                                }

                                ServerResponse::SetupSuccess(info) => {
                                    if let Some(tx) = setup_waiter.lock().await.take() {
                                        let _ = tx.send(info);
                                    } else {
                                        ui::log_warn("收到 SetupSuccess 但没有等待者");
                                    }
                                }

                                ServerResponse::StartSuccess(info) => {
                                    if let Some(tx) = start_waiter.lock().await.take() {
                                        let _ = tx.send(info);
                                    } else {
                                        ui::log_warn("收到 StartSuccess 但没有等待者");
                                    }
                                }

                                ServerResponse::ChunkReadyForDownload { chunk_id, passphrase_b64 } => {
                                    let ev = DownloadEvent::ChunkReady { chunk_id, passphrase_b64 };
                                    if let Err(e) = download_tx.send(ev).await {
                                        ui::log_error(&format!("发送 DownloadEvent 失败: {e}"));
                                    }
                                }

                                ServerResponse::UploadCompleted => {
                                    if let Err(e) = download_tx.send(DownloadEvent::UploadCompleted).await {
                                        ui::log_error(&format!("发送 UploadCompleted 失败: {e}"));
                                    }
                                }

                                ServerResponse::Error(msg) => {
                                    return Err(anyhow!("服务端错误: {msg}"));
                                }
                            }
                        },
                        None => {
                            ui::log_debug("response_rx 已关闭, dispatcher 退出");
                            return Ok(());
                        }
                    }
                }

                // 每秒定期检查心跳超时
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if last_heartbeat.elapsed() > timeout {
                        return Err(anyhow!("心跳超时: 5 秒未收到服务端心跳，连接可能已断开"));
                    }
                }
            }
        }
    }

    pub fn spawn_dispatcher(&mut self) {
        let response_rx = self.response_rx.take().expect("dispatcher 已经被启动过了");

        let download_tx = self.download_tx.clone();
        let ready_waiter = Arc::clone(&self.ready_waiter);
        let setup_waiter = Arc::clone(&self.setup_waiter);
        let start_waiter = Arc::clone(&self.start_waiter);

        tokio::spawn(async move {
            if let Err(e) = GMFClient::run_dispatcher(
                response_rx,
                download_tx,
                ready_waiter,
                setup_waiter,
                start_waiter,
            )
            .await
            {
                ui::log_error(&format!("dispatcher 发生错误: {e:#}"));
            }
        });
    }

    pub async fn wait_ready(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();

        {
            let mut guard = self.ready_waiter.lock().await;
            *guard = Some(tx);
        }

        timeout(Duration::from_secs(10), rx)
            .await
            .context("服务端启动失败，请使用 -v 重新运行并检查服务端日志")??;

        Ok(())
    }

    pub async fn setup(&mut self) -> Result<()> {
        let file_path = self.args.path.clone();
        let chunk_size = self.args.chunk_size;

        // 创建一个 oneshot 用来等待 SetupSuccess
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.setup_waiter.lock().await;
            *guard = Some(tx);
        }

        ui::log_info("正在取得文件信息...");
        self.send_request(ClientRequest::Setup {
            path: file_path.to_string(),
            chunk_size,
        })
        .await?;

        // 在这里等待 dispatcher 把结果发回来
        let info = rx.await.context("等待 SetupSuccess 被取消或连接关闭")?;

        let success_msg = format!(
            "文件名称: {} (大小: {})",
            info.file_name,
            format_size(info.file_size)
        );
        ui::log_info(&success_msg);

        let (session, completed_chunks) = DownloadSession::new(
            &info.file_name,
            info.file_size,
            &info.xxh3,
            &file_path,
            chunk_size,
            info.total_chunks,
        )?;

        ui::init_global_download_bar(info.total_chunks, completed_chunks)?;

        self.resume_from_chunk_id = completed_chunks;
        self.download_session = Some(Arc::new(session));

        Ok(())
    }

    pub async fn start(&mut self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.start_waiter.lock().await;
            *guard = Some(tx);
        }

        ui::log_debug("正在发送 Start 请求...");
        self.send_request(ClientRequest::Start {
            resume_from_chunk_id: self.resume_from_chunk_id,
        })
        .await?;

        // 等待 StartSuccess
        let start_info = rx.await.context("等待 StartSuccess 时被取消或连接关闭")?;
        let remaining_size = start_info.remaining_size;

        ui::log_debug(&format!(
            "服务端确认，需要传输的大小为 {} ，开始接收分块信息...",
            format_size(remaining_size)
        ));

        // 开始下载循环
        match self.download_loop().await {
            Ok(upload_time) => {
                if let Some(session) = &self.download_session {
                    session
                        .finalize()
                        .await
                        .context("下载完成后整理本地文件失败（重命名或删除元数据失败）")?;
                }
                let secs = upload_time.as_secs_f64();
                let speed_str = if secs > 0.0 {
                    format!(
                        "{:.2} MB/s",
                        (remaining_size as f64 / secs) / (1024.0 * 1024.0)
                    )
                } else {
                    "N/A".to_string()
                };
                println!("下载完成，平均传输速度: {speed_str}");
                Ok(())
            }
            Err(e) => {
                ui::abandon_download();
                Err(e)
            }
        }
    }

    /// 接收服务端消息的主循环
    async fn download_loop(&mut self) -> Result<Duration> {
        let download_session = self.download_session.as_ref().unwrap().clone();
        let total_chunks = download_session.total_chunks().await;
        let mut completed_chunks = download_session.completed_chunks().await;
        let mut join_set: JoinSet<ChunkResult> = JoinSet::new();
        let mut upload_completed = false;
        let mut upload_time = Duration::ZERO;
        let start_time = std::time::Instant::now();

        ui::start_tick();

        ui::log_debug(&format!(
            "下载事件循环开始，总分块数: {total_chunks}, 已完成分块数: {completed_chunks}, 需要下载分块数: {}",
            total_chunks - completed_chunks
        ));

        loop {
            // 检查是否所有分块都已完成
            if completed_chunks == total_chunks {
                while join_set.join_next().await.is_some() {}
                ui::log_debug("所有分块均已成功处理！");
                ui::finish_download();
                break;
            }

            tokio::select! {
                biased;

                // 处理已完成的分块任务
                Some(res) = join_set.join_next() => {
                    match res {
                        Ok(ChunkResult::Success(chunk_id)) => {
                            ui::log_debug(&format!("分块 #{chunk_id} 处理成功"));
                            completed_chunks = download_session.completed_chunks().await;
                            ui::update_download();
                        },
                        Ok(ChunkResult::Timeout(chunk_id)) => {
                            ui::log_debug(&format!("分块 #{chunk_id} 下载超时，正在重试..."));
                            self.send_request(ClientRequest::Retry { chunk_id }).await?;
                        },
                        Ok(ChunkResult::Failure(chunk_id, err)) => {
                            bail!("分块 #{chunk_id} 处理失败: {err:?}");
                        },
                        Err(join_err) => {
                            bail!("任务执行出现严重错误 (panic): {join_err:?}");
                        },
                    }
                },

                Some(ev) = self.download_rx.recv(), if !upload_completed => {
                    match ev {
                        DownloadEvent::ChunkReady { chunk_id, passphrase_b64 } => {
                            if download_session.is_completed(chunk_id).await {
                                ui::log_debug(&format!("分块 #{chunk_id} 已完成，跳过"));
                                continue;
                            }
                            let session_clone = download_session.clone();
                            join_set.spawn(async move {
                                session_clone.handle_chunk(chunk_id, passphrase_b64).await
                            });
                        }
                        DownloadEvent::UploadCompleted => {
                            upload_time = start_time.elapsed();
                            upload_completed = true;
                            ui::log_debug("服务端上传已完成");
                        }
                    }
                }

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
            .context("向 Communicator 发送命令失败，通道可能已关闭")
    }

    pub async fn shutdown(mut self) -> Result<()> {
        // 关闭命令发送通道，这将导致 SSHCommunicator 退出
        drop(self.command_tx);

        // 等待 SSHCommunicator 任务结束
        if let Err(e) = self.comm_handle.await {
            ui::log_warn(&format!("Communicator 异常退出: {e:?}"));
        }

        // 关闭 SSH 会话，触发远程程序退出
        self.ssh_session.close().await?;

        Ok(())
    }
}
