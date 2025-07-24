use crate::config::Config;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use r2::{Manifest, TaskEvent};
use reqwest::{Client, StatusCode};
use reqwest_eventsource::{Event, EventSource};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::sleep;

include!(concat!(env!("OUT_DIR"), "/gmf-remote.rs"));

const TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

pub struct RemoteRunner {
    cfg: Config,
    pid: u32,
    url: String,
    forward_child: tokio::process::Child,
    manifest_file: Option<Manifest>,
}

impl RemoteRunner {
    // 设置文件路径
    pub async fn setup(&self, file_path: &str) -> Result<String> {
        #[derive(serde::Serialize)]
        struct Payload {
            path: String,
        }

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .context("创建 reqwest 客户端失败")?;

        let payload = Payload {
            path: file_path.to_string(),
        };
        let url = format!("{}/setup", self.url);

        let response = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("发送 setup 请求失败")?;

        let status = response.status();

        let body = response.text().await.context("读取响应体失败")?;

        if status != StatusCode::OK {
            match status {
                StatusCode::BAD_REQUEST => {
                    bail!("远程文件查找失败 (请求错误 400): {}", body);
                }
                StatusCode::INTERNAL_SERVER_ERROR => {
                    bail!("远程服务器处理失败 (错误 500): {}", body);
                }
                _ => {
                    bail!("远程请求失败，状态码: {}, 响应: {}", status, body);
                }
            }
        }
        // 这会打印文件信息
        println!("{}", body);
        Ok(body)
    }

    // 开始处理
    pub async fn start(&mut self) -> Result<()> {
        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;

        let url = format!("{}/start", self.url);

        // 创建一个通道，用于下载任务通知主循环它们已完成（多写单读）
        let (download_complete_tx, mut download_complete_rx) =
            mpsc::channel::<Result<u32, (u32, anyhow::Error)>>(128);

        // 2. 连接到 SSE 事件源
        let mut event_source =
            EventSource::new(client.get(&url).header("Accept", "text/event-stream"))?;

        // 3. 主事件循环
        let mut total_chunks = 0;
        let mut completed_chunks = HashSet::new();
        let mut server_task_completed = false;

        loop {
            tokio::select! {
                // 分支一：监听 SSE 事件
                Some(event) = event_source.next() => {
                    match event {
                        Ok(Event::Open) => {},
                        Ok(Event::Message(message)) => {
                            let task_event: TaskEvent = serde_json::from_str(&message.data)?;
                            match task_event {
                                TaskEvent::ProcessingStart => {
                                    println!("服务端开始处理");
                                }
                                TaskEvent::SplitComplete { manifest } => {
                                    println!("服务端已完成分块加密。总分块数: {}", manifest.total_chunks);
                                    total_chunks = manifest.total_chunks;
                                    self.manifest_file = Some(manifest);
                                }
                                TaskEvent::ChunkReadyForDownload { chunk_id, remote_path } => {
                                    println!("分块 {} 已就绪，准备下载: {}", chunk_id, remote_path);

                                    // 派生一个新任务去下载并确认
                                    let task_client = client.clone();
                                    let ack_url = format!("{}/acknowledge/{}", self.url, chunk_id);
                                    let tx_clone = download_complete_tx.clone();

                                    tokio::spawn(async move {
                                        // 下载文件
                                        println!("[下载任务 {}] 开始下载...", chunk_id);
                                        sleep(Duration::from_secs(3)).await; // 模拟下载延迟
                                        println!("[下载任务 {}] 下载完成。", chunk_id);

                                        println!("[下载任务 {}] 发送确认...", chunk_id);

                                        let ack_result = task_client.post(&ack_url).send().await;
                                         if let Err(e) = ack_result {
                                            let _ = tx_clone.send(Err((chunk_id, e.into()))).await;
                                            return;
                                        }
                                        println!("[下载任务 {}] 确认成功。", chunk_id);

                                        // 通知主循环此分块已完成
                                        let _ = tx_clone.send(Ok(chunk_id)).await;
                                    });
                                }
                                TaskEvent::TaskCompleted => {
                                    println!("服务端报告任务已全部完成！");
                                    server_task_completed = true;
                                }
                                TaskEvent::Error { message } => {
                                    event_source.close();
                                    return Err(anyhow!("服务端错误: {}", message));
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            return Err(anyhow!("SSE 流错误: {}", e));
                        }
                    }
                }

                // 分支二：处理已完成的下载任务
                Some(download_result) = download_complete_rx.recv() => {
                    match download_result {
                        Ok(chunk_id) => {
                            completed_chunks.insert(chunk_id);
                            println!("进度: {} / {}", completed_chunks.len(), total_chunks);
                        }
                        Err((chunk_id, e)) => {
                             eprintln!("处理分块 {} 时发生错误: {}", chunk_id, e);
                             // todo:重试
                             event_source.close();
                             return Err(e);
                        }
                    }
                }
            }

            // 检查退出条件
            if server_task_completed
                && total_chunks > 0
                && completed_chunks.len() == total_chunks as usize
            {
                println!("所有分块已成功下载和确认！");
                break;
            }
        }

        event_source.close();
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
        add_ssh_args(&mut cmd, &cfg)
            .arg("~/.local/bin/gmf-remote")
            .env("ENDPOINT", cfg.endpoint.clone())
            .env("ACCESS_KEY_ID", cfg.access_key_id.clone())
            .env("SECRET_ACCESS_KEY", cfg.secret_access_key.clone())
            .kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        cmd.spawn().context("无法启动 ssh 进程 (gmf-remote)")?
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
        add_ssh_args(&mut cmd, &cfg)
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
        manifest_file: None,
    })
}

async fn ensure_cmd_exists(cfg: &Config, cmd: &str) -> Result<()> {
    ssh_once(cfg, &format!("command -v {} >/dev/null 2>&1", cmd))
        .await
        .context(format!(
            "远程服务器上未找到命令 `{}`，请先安装或配置 PATH",
            cmd
        ))
        .map(|_| ())
}

//------------------------------------------------------------
// 内部：文件校验 / 上传
//------------------------------------------------------------
async fn ensure_remote(cfg: &Config) -> Result<()> {
    ssh_once(cfg, "ls").await.context("远程服务器连接失败")?;
    println!("远程服务器连接成功");

    for cmd in &["sha256sum", "cut", "gunzip"] {
        ensure_cmd_exists(cfg, cmd).await?;
    }

    let sha = ssh_once(
        cfg,
        "sha256sum ~/.local/bin/gmf-remote 2>/dev/null | cut -d' ' -f1",
    )
    .await?;
    if sha.trim() == REMOTE_ELF_SHA256 {
        return Ok(());
    }

    // 上传 gzip
    println!("正在安装 gmf-remote 至 ~/.local/bin 目录");
    let tmp = std::env::temp_dir().join("gmf-remote.gz");
    tokio::fs::write(&tmp, REMOTE_ELF_GZ).await?;
    scp_send(cfg, &tmp, "~/gmf-remote.gz").await?;
    tokio::fs::remove_file(&tmp).await.ok();

    // 解压 + chmod
    let cmd = r#"
        mkdir -p ~/.local/bin &&
        gunzip -c ~/gmf-remote.gz > ~/.local/bin/gmf-remote &&
        chmod +x ~/.local/bin/gmf-remote &&
        rm ~/gmf-remote.gz
    "#;
    ssh_once(cfg, cmd).await?;

    // 再次校验
    let new_sha = ssh_once(cfg, "sha256sum ~/.local/bin/gmf-remote | cut -d' ' -f1").await?;
    if new_sha.trim() != REMOTE_ELF_SHA256 {
        Err(anyhow!(
            "安装失败：数据完整性校验失败：实际值 {} 与期望值 {REMOTE_ELF_SHA256} 不符",
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
    // 判断私钥路径字符串是否为空
    if !cfg.private_key_path.is_empty() {
        // 如果不为空，则返回包含 "-i" 和路径的 Vec
        vec!["-i".to_string(), cfg.private_key_path.clone()]
    } else {
        // 如果为空，则返回一个空的 Vec
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
    drop(listener); // 立即释放，后续 ssh -L 会占用
    Ok(port)
}
