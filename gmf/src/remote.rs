use crate::config::Config;
use crate::gmf_file::{ChunkResult, GMFFile, GmfSession};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use gmf_common::{SetupRequestPayload, SetupResponse, StartRequestPayload, TaskEvent};
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;

use tokio::time::sleep;

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

const TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(500);
const SSE_TIMEOUT: Duration = Duration::from_secs(20);

pub struct RemoteRunner {
    cfg: Config,
    pid: u32,
    url: String,
    forward_child: tokio::process::Child,
    session: Option<GmfSession>,
    completed_chunks: u32,
}

impl RemoteRunner {
    pub async fn setup(&mut self, file_path: &str, chunk_size: usize) -> Result<()> {
        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(10))
            .build()
            .context("创建 reqwest 客户端失败")?;

        let payload = SetupRequestPayload {
            path: file_path.to_string(),
            chunk_size,
        };
        let url = format!("{}/setup", self.url);

        let response = client
            .post(&url)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        let setup_info = response.json::<SetupResponse>().await?;

        println!("文件名: {}, 大小: {}", setup_info.filename, setup_info.size);

        let (gmf_file, completed_chunks) = GMFFile::new(
            &setup_info.filename,
            setup_info.size,
            &setup_info.sha256,
            setup_info.total_chunks,
        )?;

        // 存储已完成的分块数
        self.completed_chunks = completed_chunks;

        let session = GmfSession::new(gmf_file, self.completed_chunks);
        self.session = Some(session);

        Ok(())
    }

    // 开始处理
    pub async fn start(&mut self) -> Result<()> {
        let session = self
            .session
            .take()
            .ok_or_else(|| anyhow!("必须先调用 setup() 方法才能开始任务"))?;

        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;
        let sse_url = format!("{}/start", self.url);

        // 服务端 chunk_id 从 1 开始，而我们的 completed_chunks 是 0-based 计数
        // 如果 completed_chunks 是 5，表示 0,1,2,3,4 已完成，下一个需要的是 5 (对应服务器的 chunk_id 6)
        // 所以，请求服务器从 completed_chunks + 1 开始发送。
        let start_payload = StartRequestPayload {
            resume_from_chunk_id: self.completed_chunks + 1,
        };

        let mut event_source = EventSource::new(
            client
                .post(&sse_url)
                .header("Accept", "text/event-stream")
                .json(&start_payload),
        )?;

        println!("等待服务端事件...");
        if self.completed_chunks > 0 {
            println!("请求从分块 ID {} 继续传输。", self.completed_chunks + 1);
        }

        let mut worker_handles = Vec::new();

        loop {
            tokio::select! {
                Some(event) = event_source.next() => {
                    match event {
                        Ok(Event::Message(message)) => {
                            let task_event: TaskEvent = serde_json::from_str(&message.data)
                                .context("解析服务端事件失败")?;

                            if let TaskEvent::ChunkReadyForDownload { chunk_id, passphrase_b64 } = task_event {
                                // 如果这个块已经下载过了，就跳过
                                // 注意：服务端 chunk_id 是 1-based
                                if chunk_id <= self.completed_chunks {
                                    continue;
                                }
                                let handle = session.handle_chunk(
                                    chunk_id,
                                    passphrase_b64,
                                );
                                worker_handles.push(handle);

                            } else if let TaskEvent::TaskCompleted = task_event {
                                println!("服务端报告任务完成，等待所有本地 worker 结束...");
                                event_source.close();
                                break;
                            } else if let TaskEvent::Error { message } = task_event {
                                bail!("服务端错误: {}", message);
                            }
                        }
                        Err(e) => {
                            return Err(anyhow!("SSE 流意外断开: {}", e));
                        }
                        _ => {}
                    }
                }
                 _ = sleep(SSE_TIMEOUT) => {
                    // 如果已经有完成的分块，并且没有新的 worker 在运行，这可能是正常的，因为我们可能在等待服务器跳过已完成的块
                    // 只有当一个 worker 都没有启动过，才认为是超时
                    if worker_handles.is_empty() && self.completed_chunks == 0 {
                        bail!("SSE 事件流超时：超过 {:?} 未收到任何事件", SSE_TIMEOUT);
                    }
                 }
            }
        }

        for handle in worker_handles {
            match handle.await? {
                ChunkResult::Success(_id) => {},
                ChunkResult::Failure(id, e) => return Err(e.context(format!("Worker {id} 失败"))),
            }
        }

        println!("所有 worker 已完成。现在等待文件写入器完成...");
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

    println!("gmf-remote 启动成功");

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
    let pid: u32 = pid_line.trim().parse().context("PID 解析失败")?;

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

    println!("gmf-remote 连接成功");

    Ok(RemoteRunner {
        cfg: cfg.clone(),
        pid,
        url,
        forward_child,
        session: None,
        completed_chunks: 0,
    })
}

async fn ensure_cmd_exists(cfg: &Config, cmd: &str) -> Result<()> {
    ssh_once(cfg, &format!("command -v {cmd} >/dev/null 2>&1"))
        .await
        .context(format!(
            "远程服务器上未找到命令 `{cmd}`，请先安装或配置 PATH"
        ))
        .map(|_| ())
}

async fn ensure_remote(cfg: &Config) -> Result<()> {
    ssh_once(cfg, "ls").await.context("远程服务器连接失败")?;
    println!("远程服务器 {} 连接成功", cfg.host);

    for cmd in &["sha256sum", "cut", "tar"] {
        ensure_cmd_exists(cfg, cmd).await?;
    }

    let sha = ssh_once(
        cfg,
        "sha256sum ~/.local/bin/gmf-remote 2>/dev/null | cut -d' ' -f1",
    )
    .await?;
    if sha.trim() == GMF_REMOTE_SHA256 {
        return Ok(());
    }

    // 上传 gzip
    println!("正在安装 gmf-remote 至 ~/.local/bin 目录");
    let tmp = std::env::temp_dir().join("gmf-remote.tar.gz");
    tokio::fs::write(&tmp, GMF_REMOTE_TAR_GZ).await?;
    scp_send(cfg, &tmp, "~/gmf-remote.tar.gz").await?;
    tokio::fs::remove_file(&tmp).await.ok();

    // 解压 + chmod
    let cmd = r#"
        mkdir -p ~/.local/bin &&
        tar -xzf ~/gmf-remote.tar.gz -C ~/.local/bin &&
        chmod +x ~/.local/bin/gmf-remote &&
        rm ~/gmf-remote.tar.gz
    "#;
    ssh_once(cfg, cmd).await?;

    // 再次校验
    let new_sha = ssh_once(cfg, "sha256sum ~/.local/bin/gmf-remote | cut -d' ' -f1").await?;
    if new_sha.trim() != GMF_REMOTE_SHA256 {
        Err(anyhow!(
            "安装失败：数据完整性校验失败：实际值 {} 与期望值 {GMF_REMOTE_SHA256} 不符",
            new_sha.trim()
        ))
    } else {
        println!("gmf-remote 安装成功");
        Ok(())
    }
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
