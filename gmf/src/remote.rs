use crate::config::Config;
use crate::file::{ChunkResult, GMFFile, GmfSession};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use gmf_common::{SetupRequestPayload, SetupResponse, StartRequestPayload, TaskEvent, format_size};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{debug, info};
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::sleep;

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

const TIMEOUT: Duration = Duration::from_secs(20);
const RETRY_INTERVAL: Duration = Duration::from_millis(500);
const SSE_TIMEOUT: Duration = Duration::from_secs(20);

pub struct TaskContext {
    progress_bar: AllProgressBar,
    session: GmfSession,
}

impl TaskContext {
    pub fn new(info: &SetupResponse) -> Result<Self> {
        let (gmf_file, completed_chunks) =
            GMFFile::new(&info.filename, info.size, &info.sha256, info.total_chunks)?;
        let progress_bar = AllProgressBar::new(info.total_chunks, completed_chunks)?;
        let session = GmfSession::new(gmf_file, completed_chunks);

        Ok(TaskContext {
            progress_bar,
            session,
        })
    }
}

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

pub struct RemoteRunner {
    cfg: Config,
    pid: u64,
    url: String,
    forward_child: tokio::process::Child,
    session: Option<GmfSession>,
    completed_chunks: u64,
    multi_progress: MultiProgress,
    upload_pb: ProgressBar,
    chunk_size: Option<u64>,
}

impl RemoteRunner {
    pub async fn setup(
        &mut self,
        file_path: &str,
        chunk_size: u64,
        concurrency: u64,
    ) -> Result<()> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()?;

        let payload = SetupRequestPayload {
            path: file_path.to_string(),
            chunk_size,
            concurrency,
        };
        let url = format!("{}/setup", self.url);

        info!("正在取得文件信息...");
        let response = client
            .post(&url)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        let setup_info = response.json::<SetupResponse>().await?;

        info!(
            "文件名: {}, 大小: {}",
            setup_info.filename,
            format_size(setup_info.size)
        );

        let (gmf_file, completed_chunks) = GMFFile::new(
            &setup_info.filename,
            setup_info.size,
            &setup_info.sha256,
            setup_info.total_chunks,
        )?;

        // 存储已完成的分块数
        self.completed_chunks = completed_chunks;
        self.chunk_size = Some(chunk_size as u64);

        self.upload_pb = self
            .multi_progress
            .add(ProgressBar::new(setup_info.total_chunks as u64));
        self.upload_pb.set_style(
            ProgressStyle::default_bar()
                .template("上传进度 [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        self.upload_pb.set_position(completed_chunks as u64);

        let session = GmfSession::new(gmf_file, self.completed_chunks);
        self.session = Some(session);

        Ok(())
    }

    // 开始处理
    pub async fn start(&mut self) -> Result<()> {
        let session = self.session.take().unwrap();

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(20))
            .build()?;
        let sse_url = format!("{}/start", self.url);

        let start_payload = StartRequestPayload {
            resume_from_chunk_id: self.completed_chunks,
        };

        let mut event_source = EventSource::new(
            client
                .post(&sse_url)
                .header("Accept", "text/event-stream")
                .json(&start_payload),
        )?;

        let mut worker_handles = FuturesUnordered::new();
        let mut task_completed_signal = false; // 用于标记是否收到了服务端的完成信号

        loop {
            tokio::select! {
            // 偏向于优先处理已完成的任务，避免积压
                biased;

                // 分支1: 处理已完成的 worker 任务
                // 当 worker_handles 不为空时，这个分支才会被轮询
                Some(result) = worker_handles.next(), if !worker_handles.is_empty() => {
                    // 明确告诉编译器 result 的类型
                    let result: Result<ChunkResult, tokio::task::JoinError> = result;

                    match result {
                        Ok(ChunkResult::Success(_id)) => {
                        }
                        Ok(ChunkResult::Failure(id, e)) => {
                        }
                        Err(join_error) => {
                            if join_error.is_panic() {
                                let panic_payload = join_error.into_panic();
                                if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                                    bail!("一个 Worker 任务发生 Panic: {s:?}");
                                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                                    bail!("一个 Worker 任务发生 Panic: {s:?}");
                                } else {
                                    bail!("一个 Worker 任务发生未知类型的 Panic");
                                }
                            } else {
                                return Err(anyhow!("Worker 任务被终止: {}", join_error));
                            }
                        }
                    }
                }

                // 分支2: 从服务端接收新任务
                // 仅当服务端尚未发出完成信号时，才接收新事件
                event = event_source.next(), if !task_completed_signal => {
                    match event {
                        Some(Ok(Event::Message(message))) => {
                            let task_event: TaskEvent = serde_json::from_str(&message.data)
                                .context("解析服务端事件失败")?;

                            if let TaskEvent::ChunkReadyForDownload { chunk_id, passphrase_b64 } = task_event {
                                if chunk_id < self.completed_chunks {
                                    continue;
                                }
                                self.upload_pb.inc(1);
                                let handle = session.handle_chunk(chunk_id, passphrase_b64);
                                // 将新任务推入 FuturesUnordered
                                worker_handles.push(handle);
                            } else if let TaskEvent::TaskCompleted = task_event {
                                self.upload_pb.finish();
                                task_completed_signal = true; // 标记服务端任务已全部派发
                                event_source.close();
                            } else if let TaskEvent::Error { message } = task_event {
                                bail!("服务端错误: {}", message);
                            }
                        }
                        Some(Err(e)) => {
                            self.upload_pb.abandon();
                            return Err(anyhow!("SSE 流意外断开: {}", e));
                        }
                        None => {
                            // SSE 流正常关闭
                            task_completed_signal = true;
                        }
                        _ => {}
                    }
                }

                // 分支3: 超时检查
                _ = sleep(SSE_TIMEOUT), if worker_handles.is_empty() && !task_completed_signal => {
                        if self.completed_chunks == 0 {
                        bail!("SSE 事件流超时：超过 {:?} 未收到任何事件", SSE_TIMEOUT);
                        }
                }

                // 循环退出条件
                else => {
                    // 当其他分支都无法再取得进展时，进入此分支
                    // 如果服务端已发完任务，并且所有 worker 都已处理完毕，则退出循环
                    if task_completed_signal && worker_handles.is_empty() {
                        break;
                    }
                }
            }
        }

        debug!("所有 worker 已完成。现在等待文件写入器完成...");
        session.wait_for_completion().await?;

        Ok(())
    }

    // 关闭远程服务
    pub async fn shutdown(&mut self) -> Result<()> {
        self.forward_child
            .kill()
            .await
            .context("无法关闭 ssh 端口转发进程")?;

        let cfg = &self.cfg;
        let output = Command::new("ssh")
            .arg("-T")
            .arg("-p")
            .arg(cfg.port.to_string())
            .args(private_key_args(cfg))
            .arg(format!("{}@{}", cfg.user, cfg.host))
            .arg(format!("kill {}", self.pid))
            .output()
            .await
            .context("执行远程 kill 命令失败")?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "远程 kill 命令返回非零状态 ({}): {}",
                output.status,
                stderr.trim()
            )
        }
    }
}

// 启动远端并返回 Runner
pub async fn start_remote(cfg: &Config) -> Result<RemoteRunner> {
    ensure_remote(cfg).await?;

    debug!("正在启动 gmf-remote...");
    // 启动 gmf‑remote
    let mut ssh_child = {
        let mut cmd = Command::new("ssh");
        let remote_command = format!(
            "ENDPOINT='{}' ACCESS_KEY_ID='{}' SECRET_ACCESS_KEY='{}' ~/.local/bin/gmf-remote",
            cfg.endpoint, cfg.access_key_id, cfg.secret_access_key,
        );
        add_ssh_args(&mut cmd, cfg)
            .arg(&remote_command)
            .kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        cmd.spawn().context("无法启动 gmf-remote")?
    };

    debug!("gmf-remote 启动成功");

    // 获取输出
    let stdout = ssh_child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("无法获得 ssh stdout"))?;
    let mut reader = BufReader::new(stdout).lines();

    // 读取 PID
    let pid_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("gmf-remote 未输出 PID"))?;
    let pid: u64 = pid_line.trim().parse().context("PID 解析失败")?;

    // 读取端口
    let port_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("gmf-remote 未输出端口号"))?;
    let remote_port: u16 = port_line.trim().parse().context("端口号解析失败")?;

    // 建立本地端口转发：local_port -> 127.0.0.1:remote_port
    let local_port = pick_free_port().await?;
    let mut forward_child = {
        let mut cmd = Command::new("ssh");
        add_ssh_args(&mut cmd, cfg)
            .args(["-N", "-o", "ExitOnForwardFailure=yes"])
            .arg("-L")
            .arg(format!("{local_port}:127.0.0.1:{remote_port}"));
        cmd.spawn().context("无法启动 ssh 端口转发进程")?
    };

    // 轮询本地端口是否就绪
    let url = format!("https://127.0.0.1:{local_port}");
    let client = Client::builder()
        .timeout(TIMEOUT)
        .danger_accept_invalid_certs(true)
        .build()?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status() == 200 => break,
            _ if Instant::now() > deadline => {
                ssh_child.kill().await.ok();
                forward_child.kill().await.ok();
                return Err(anyhow!("等待端口 {local_port} 就绪超时 (> {TIMEOUT:?})"));
            }
            _ => sleep(RETRY_INTERVAL).await,
        }
    }

    info!("gmf-remote 连接成功");
    let mp = MultiProgress::new();
    Ok(RemoteRunner {
        cfg: cfg.clone(),
        pid,
        url,
        forward_child,
        session: None,
        completed_chunks: 0,
        multi_progress: mp,
        upload_pb: ProgressBar::new(0),
        chunk_size: None,
    })
}

async fn ensure_remote(cfg: &Config) -> Result<()> {
    info!("正常尝试连接远程服务器 {}", cfg.host);
    ssh_once(cfg, "ls").await.context("远程服务器连接失败")?;
    info!("远程服务器 {} 连接成功", cfg.host);

    let check_cmd = r#"
        set -e
        for cmd in sha256sum cut tar; do
            command -v "$cmd" >/dev/null 2>&1
        done
        FILE_PATH=~/.local/bin/gmf-remote
        if [ -f "$FILE_PATH" ]; then
            sha256sum "$FILE_PATH" | cut -d' ' -f1
        fi
    "#;

    let current_sha = ssh_once(cfg, check_cmd)
        .await
        .context("远程服务器初始检查失败 (可能缺少依赖命令如 sha256sum, cut, tar)")?;

    // 在 Rust 端进行判断
    if current_sha.trim() == GMF_REMOTE_SHA256 {
        return Ok(());
    }

    // 上传 gzip
    info!("正在安装 gmf-remote 至 ~/.local/bin 目录");
    let tmp = std::env::temp_dir().join("gmf-remote.tar.gz");
    tokio::fs::write(&tmp, GMF_REMOTE_TAR_GZ).await?;
    scp_send(cfg, &tmp, "~/gmf-remote.tar.gz").await?;
    tokio::fs::remove_file(&tmp).await.ok();

    // 解压 + chmod
    let install_and_verify_cmd = format!(
        r#"
        # 任何命令失败则立即退出
        set -e

        # 定义常量和路径
        BIN_DIR=~/.local/bin
        REMOTE_FILE="$BIN_DIR/gmf-remote"
        ARCHIVE=~/gmf-remote.tar.gz
        EXPECTED_SHA="{}"

        # 核心操作
        mkdir -p "$BIN_DIR"
        tar -xzf "$ARCHIVE" -C "$BIN_DIR"
        chmod +x "$REMOTE_FILE"
        rm "$ARCHIVE"

        # 最终校验
        ACTUAL_SHA=$(sha256sum "$REMOTE_FILE" | cut -d' ' -f1)

        # 如果校验失败，直接以非零状态码退出
        [ "$ACTUAL_SHA" = "$EXPECTED_SHA" ]
    "#,
        GMF_REMOTE_SHA256
    );

    ssh_once(cfg, &install_and_verify_cmd)
        .await
        .context("在远程服务器上安装或校验 gmf-remote 时发生错误")?;

    info!("gmf-remote 安装成功");
    Ok(())
}

fn add_ssh_args<'a>(cmd: &'a mut Command, cfg: &Config) -> &'a mut Command {
    cmd.arg("-p")
        .arg(cfg.port.to_string())
        .args(private_key_args(cfg))
        .arg(format!("{}@{}", cfg.user, cfg.host))
}

fn private_key_args(cfg: &Config) -> Vec<String> {
    if !cfg.private_key_path.is_empty() {
        vec!["-i".to_string(), cfg.private_key_path.clone()]
    } else {
        Vec::new()
    }
}

// TODO：使用 russh-sftp 重构
async fn scp_send(cfg: &Config, local: &Path, remote: &str) -> Result<()> {
    let status = {
        let mut cmd = Command::new("scp");
        cmd.arg("-q")
            .arg("-P")
            .arg(cfg.port.to_string())
            .args(private_key_args(cfg))
            .arg(local)
            .arg(format!("{}@{}:{remote}", cfg.user, cfg.host));
        cmd.status().await?
    };
    status
        .success()
        .then_some(())
        .ok_or_else(|| anyhow!("scp 上传失败"))
}

// TODO: 使用 russh 重构
async fn ssh_once(cfg: &Config, remote_cmd: &str) -> Result<String> {
    let output = {
        let mut cmd = Command::new("ssh");
        add_ssh_args(&mut cmd, cfg).arg(remote_cmd);
        cmd.output().await?
    };
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(anyhow!(
            "ssh `{remote_cmd}` 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

// 获取一个可用的本地端口
async fn pick_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}
