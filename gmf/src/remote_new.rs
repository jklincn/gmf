use crate::config::Config;
use crate::file::GmfSession;
use crate::io_actor::IoActor;
use crate::ssh::{self};
use anyhow::{Context, Result, anyhow};
use gmf_common::interface::*;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{error, info, warn};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

// pub struct TaskContext {
//     progress_bar: AllProgressBar,
//     session: GmfSession,
// }

// impl TaskContext {
//     pub fn new(info: &SetupResponse) -> Result<Self> {
//         let (gmf_file, completed_chunks) =
//             GMFFile::new(&info.filename, info.size, info.total_chunks)?;
//         let progress_bar = AllProgressBar::new(info.total_chunks, completed_chunks)?;
//         let session = GmfSession::new(gmf_file, completed_chunks);

//         Ok(TaskContext {
//             progress_bar,
//             session,
//         })
//     }
// }

pub struct AllProgressBar {
    upload: ProgressBar,
    download: ProgressBar,
    writer: ProgressBar,
}

impl AllProgressBar {
    pub fn new(total_chunks: u64, completed_chunks: u64) -> Result<Self> {
        let mp = MultiProgress::new();
        let upload = mp.add(ProgressBar::new(total_chunks));
        let download = mp.add(ProgressBar::new(total_chunks));
        let writer = mp.add(ProgressBar::new(total_chunks));

        upload.set_style(
            ProgressStyle::default_bar()
                .template("上传进度 [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        download.set_style(
            ProgressStyle::default_bar()
                .template("下载进度 [{bar:40.green/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        writer.set_style(
            ProgressStyle::default_bar()
                .template("写入进度 [{bar:40.green/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        upload.set_position(completed_chunks);
        download.set_position(completed_chunks);
        writer.set_position(completed_chunks);

        Ok(AllProgressBar {
            upload,
            download,
            writer,
        })
    }

    pub fn update_upload(&self) {
        self.upload.inc(1);
    }
    pub fn update_download(&self) {
        self.download.inc(1);
    }
    pub fn update_writer(&self) {
        self.writer.inc(1);
    }
}

pub async fn check_remote(cfg: &Config) -> Result<(ssh::Session, String)> {
    info!("正在尝试连接远程服务器 {}:{}", cfg.host, cfg.port);
    let mut ssh = ssh::Session::connect(cfg).await?;
    info!("远程服务器连接成功");

    const REMOTE_BIN_DIR: &str = "$HOME/.local/bin";
    const REMOTE_PATH: &str = "$HOME/.local/bin/gmf-remote";

    // 检查远程程序 SHA256 的命令
    let check_sha_cmd = format!(
        r#"
        if [ -f "{}" ]; then
            sha256sum "{}" | cut -d' ' -f1
        else
            echo ""
        fi
    "#,
        REMOTE_PATH, REMOTE_PATH
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
        info!("正在安装 gmf-remote 至远程目录: {}", REMOTE_BIN_DIR);

        // 创建远程目录
        ssh.call_once(&format!("mkdir -p {}", REMOTE_BIN_DIR))
            .await?;

        // 上传并解压
        ssh.untar_from_memory(GMF_REMOTE_TAR_GZ, REMOTE_BIN_DIR)
            .await?;

        info!("gmf-remote 安装成功");
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
    progress_bar: Option<AllProgressBar>,
    // GMF 文件管理
    file: Option<GmfSession>,
}

impl InteractiveSession {
    /// 启动远程程序并创建一个新的交互式会话。
    pub async fn start(config: &Config) -> Result<Self> {
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
        // 做一个检查
        match self.next_response().await? {
            Some(response) => {
                if let ServerResponse::Ready = response {
                    info!("远程程序已准备就绪");
                }
            }
            _ => {
                return Err(anyhow!("未收到 Setup 响应"));
            }
        }

        let client_request = ClientRequest::Setup(SetupRequestPayload {
            path: file_path.to_string(),
            chunk_size,
            concurrency,
        });

        info!("正在取得文件信息...");
        self.send_request(&client_request).await?;

        match self.next_response().await? {
            Some(response) => {
                if let ServerResponse::SetupSuccess(setup_response) = response {
                    info!(
                        "文件 {} 已准备就绪，大小: {} 字节，总分片数: {}",
                        setup_response.file_name,
                        setup_response.file_size,
                        setup_response.total_chunks
                    );
                }
            }
            _ => {
                return Err(anyhow!("未收到 Setup 响应"));
            }
        }

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
            Err(_) => Err(anyhow::anyhow!("等待响应超时 (超过 5秒)",)),
        }
    }

    /// 发送退出指令，并等待程序结束。
    pub async fn shutdown(mut self) -> Result<()> {
        info!("开始执行优雅关闭流程...");

        // 步骤 1: 停止发送新命令
        // 通过拿走 self 的所有权并 drop command_tx，来通知 IoActor 不会再有新任务。
        // IoActor 的 command_rx.recv() 会返回 None，使其退出主循环。
        drop(self.command_tx);
        info!("命令通道已关闭。");

        // 步骤 2: 等待后台 I/O Actor 任务处理完所有缓冲并正常退出
        // 我们给它一个合理的超时时间，以防它卡住。
        match tokio::time::timeout(Duration::from_secs(5), self.actor_handle).await {
            Ok(Ok(_)) => info!("I/O Actor 已成功退出。"),
            Ok(Err(e)) => error!("等待 I/O Actor 退出时发生错误: {:?}", e),
            Err(_) => warn!("等待 I/O Actor 退出超时！"),
        }

        // 步骤 3: 现在 Actor 已经停止，可以安全地、主动地关闭 SSH 连接
        // 这里调用您自己封装的 ssh::Session::close 方法
        info!("正在主动关闭 SSH 连接...");
        self.ssh_session.close().await?;
        info!("SSH 连接已成功关闭。");

        Ok(())
    }
}
