use crate::config::{Config, load_or_create_config};
use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use r2::{Manifest, TaskEvent};
use reqwest::{Client, StatusCode};
use reqwest_eventsource::{Event, EventSource};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
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
    pub url: String,
    manifest_file: Option<Manifest>,
}

#[derive(serde::Serialize)]
struct SetupPayload {
    path: String,
}

impl RemoteRunner {
    pub async fn setup(&self, file_path: &str) -> Result<()> {
        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;

        let payload = SetupPayload {
            path: file_path.to_string(),
        };

        let url = format!("{}/setup", self.url);
        let response = client.post(&url).json(&payload).send().await?;

        match response.status() {
            StatusCode::OK => {
                let response_text = response.text().await?;
                println!("文件路径已设置: {}", response_text);
                Ok(())
            }
            // 处理其他可能的错误状态码
            status => {
                let error_text = response.text().await?;
                eprintln!("An unexpected error occurred.");
                eprintln!("   Status: {}", status);
                eprintln!("   Server response: {}", error_text);
                Err(anyhow!("Task setup failed with status: {}", status))
            }
        }
    }

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
        println!("已连接到服务端事件流...");

        // 3. 主事件循环
        let mut total_chunks = 0;
        let mut completed_chunks = HashSet::new();
        let mut server_task_completed = false;

        loop {
            tokio::select! {
                // 分支一：监听 SSE 事件
                Some(event) = event_source.next() => {
                    match event {
                        Ok(Event::Open) => println!("SSE 连接已打开。"),
                        Ok(Event::Message(message)) => {
                            let task_event: TaskEvent = serde_json::from_str(&message.data)?;
                            match task_event {
                                TaskEvent::ProcessingStart => {
                                    println!("服务端开始处理...");
                                }
                                TaskEvent::SplitComplete { manifest } => {
                                    println!("服务端准备完成，收到清单。总分块数: {}", manifest.total_chunks);
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

    /// 主动关闭远端。
    pub async fn shutdown(&mut self) -> Result<()> {
        use std::process::Command;
        let cfg = &self.cfg;
        let cmd = Command::new("ssh")
            .arg("-T")
            .arg("-p")
            .arg(cfg.port.to_string())
            .args(private_key_args(cfg))
            .arg(format!("{}@{}", cfg.user, cfg.host))
            .arg(format!("kill {}", self.pid))
            .status();
        match cmd {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!("⚠️ 远端杀 gmf-remote 返回非零状态: {}", status);
            }
            Err(err) => {
                eprintln!("❌ 无法执行远端 pkill: {}", err);
            }
        }
        Ok(())
    }
}

//------------------------------------------------------------
// 入口：启动远端并返回 Runner
//------------------------------------------------------------
pub async fn start_remote() -> Result<RemoteRunner> {
    let cfg = load_or_create_config()?;
    ensure_remote(&cfg).await?;

    //--------------------------------------------------------
    // 1. ssh 启动 gmf‑remote
    //--------------------------------------------------------
    let mut ssh_child = {
        let mut cmd = Command::new("ssh");
        add_ssh_args(&mut cmd, &cfg)
            .arg("~/.local/bin/gmf-remote")
            .env("ENDPOINT", cfg.endpoint.clone().unwrap_or_default())
            .env(
                "ACCESS_KEY_ID",
                cfg.access_key_id.clone().unwrap_or_default(),
            )
            .env(
                "SECRET_ACCESS_KEY",
                cfg.secret_access_key.clone().unwrap_or_default(),
            )
            .kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        cmd.spawn().context("无法启动 ssh 进程 (gmf-remote)")?
    };

    if let Some(mut err) = ssh_child.stderr.take() {
        tokio::spawn(async move {
            let mut buf = vec![];
            err.read_to_end(&mut buf).await.ok();
            eprintln!("ssh stderr: {}", String::from_utf8_lossy(&buf));
        });
    }
    //--------------------------------------------------------
    // 2. 读取远端 PID 与端口
    //--------------------------------------------------------
    let stdout = ssh_child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("无法获得 ssh stdout"))?;
    let mut reader = BufReader::new(stdout).lines();

    // 2.1 PID
    let pid_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("远端未输出 PID"))?;
    let pid: u32 = pid_line.trim().parse().context("PID 解析失败")?;

    // 2.2 端口
    let port_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("远端未输出端口号"))?;
    let remote_port: u16 = port_line.trim().parse().context("端口号解析失败")?;
    let local_port = remote_port;

    // todo：解决防火墙

    //--------------------------------------------------------
    // 3. 建立本地端口转发：local_port -> 127.0.0.1:remote_port
    //--------------------------------------------------------
    // let local_port = pick_free_port().await?;
    // let mut forward_child = {
    //     let mut cmd = Command::new("ssh");
    //     add_ssh_args(&mut cmd, &cfg)
    //         .args(["-N", "-o", "ExitOnForwardFailure=yes"])
    //         .arg("-L")
    //         .arg(format!("{local_port}:127.0.0.1:{remote_port}"))
    //         .kill_on_drop(true);
    //     cmd.spawn().context("无法启动 ssh 端口转发进程")?
    // };

    //--------------------------------------------------------
    // 4. 轮询本地端口是否就绪
    //--------------------------------------------------------
    // let url = format!("http://127.0.0.1:{local_port}");
    let url: String = format!("https://{}:{local_port}", &cfg.host);
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
                // forward_child.kill().await.ok();
                return Err(anyhow!("等待端口 {local_port} 就绪超时 (> {TIMEOUT:?})"));
            }
            _ => sleep(RETRY_INTERVAL).await,
        }
    }

    println!("gmf-remote 已启用，连接地址: {}", url);

    Ok(RemoteRunner {
        cfg,
        pid,
        url,
        manifest_file: None,
    })
}

//------------------------------------------------------------
// 内部：文件校验 / 上传
//------------------------------------------------------------
async fn ensure_remote(cfg: &Config) -> Result<()> {
    // To be improved
    let _ = ssh_once(cfg, "pkill -x gmf-remote || true").await?;

    let sha = ssh_once(
        cfg,
        "sha256sum ~/.local/bin/gmf-remote 2>/dev/null | cut -d' ' -f1",
    )
    .await?;
    if sha.trim() == REMOTE_ELF_SHA256 {
        return Ok(());
    }
    println!("上传远端 ELF：当前校验 {sha}，期待 {REMOTE_ELF_SHA256}");
    // 上传 gzip
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
            "远端 ELF 校验失败：期待 {REMOTE_ELF_SHA256}，得到 {}",
            new_sha.trim()
        ))
    } else {
        Ok(())
    }
}

//------------------------------------------------------------
// 内部：ssh / scp 工具
//------------------------------------------------------------
fn add_ssh_args<'a>(cmd: &'a mut Command, cfg: &Config) -> &'a mut Command {
    cmd.arg("-p")
        .arg(cfg.port.to_string())
        .args(private_key_args(cfg))
        .arg(format!("{}@{}", cfg.user, cfg.host))
}

fn private_key_args(cfg: &Config) -> Vec<String> {
    cfg.private_key_path
        .as_ref()
        .filter(|p| !p.is_empty())
        .map(|p| vec!["-i".into(), p.clone()])
        .unwrap_or_default()
}

async fn scp_send(cfg: &Config, local: &Path, remote: &str) -> Result<()> {
    let status = {
        let mut cmd = Command::new("scp");
        cmd.arg("-P")
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

//------------------------------------------------------------
// Util
//------------------------------------------------------------
async fn pick_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener); // 立即释放，后续 ssh -L 会占用
    Ok(port)
}
