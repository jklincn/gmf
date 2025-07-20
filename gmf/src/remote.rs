use crate::config::{Config, load_or_create_config};
use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use r2::ManifestFile;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::sleep;

include!(concat!(env!("OUT_DIR"), "/gmf-remote.rs"));

const TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

//------------------------------------------------------------
// 对外结构体
//------------------------------------------------------------
pub struct RemoteRunner {
    cfg: Config,
    pid: u32,
    forward_child: Child,
    local_port: u16,
    url: String,
}

impl RemoteRunner {
    //--------------------------------------------------------
    // 用户态 API
    //--------------------------------------------------------
    pub async fn split(&self, filepath: &str) -> Result<ManifestFile> {
        let url = format!(
            "{}/split_sse?path={}",
            self.url,
            urlencoding::encode(filepath)
        );
        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;

        let resp = client.get(&url).send().await?.error_for_status()?; // Propagate HTTP 错误

        let mut stream = resp.bytes_stream().eventsource();

        // 等待处理进度 & 结果
        while let Some(event) = stream.next().await {
            let event = event?;
            match event.event.as_str() {
                "status" => {
                    println!("远端处理状态：{}", event.data);
                }
                "done" => {
                    let data = event.data.trim();
                    let manifest: ManifestFile =
                        serde_json::from_str(data).map_err(|e| anyhow!("反序列化失败: {}", e))?;
                    println!("远端处理完成：{:#?}", manifest);
                    return Ok(manifest);
                }
                other => {
                    println!("收到未知 SSE 事件：{}，数据：{}", other, event.data);
                }
            }
        }

        Err(anyhow!("SSE 流意外关闭，没有收到 done 事件"))
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
    // 1. ssh 启动 gmf‑remote（前台运行，stdout 打印 PID 和端口）
    //--------------------------------------------------------
    let mut ssh_child = {
        let mut cmd = Command::new("ssh");
        add_ssh_args(&mut cmd, &cfg)
            .arg("~/.local/bin/gmf-remote")
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
    println!("收到远端 PID：{}", pid_line);
    let pid: u32 = pid_line.trim().parse().context("PID 解析失败")?;

    // 2.2 端口
    let port_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("远端未输出端口号"))?;
    println!("收到远端端口：{}", port_line);
    let remote_port: u16 = port_line.trim().parse().context("端口号解析失败")?;

    //--------------------------------------------------------
    // 3. 建立本地端口转发：local_port -> 127.0.0.1:remote_port
    //--------------------------------------------------------
    let local_port = pick_free_port().await?;
    let mut forward_child = {
        let mut cmd = Command::new("ssh");
        add_ssh_args(&mut cmd, &cfg)
            .args(["-N", "-o", "ExitOnForwardFailure=yes"])
            .arg("-L")
            .arg(format!("{local_port}:127.0.0.1:{remote_port}"))
            .kill_on_drop(true);
        cmd.spawn().context("无法启动 ssh 端口转发进程")?
    };

    //--------------------------------------------------------
    // 4. 轮询本地端口是否就绪
    //--------------------------------------------------------
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

    println!("gmf-remote 已通过 SSH 隧道暴露为 {url} (远端 {remote_port})");

    Ok(RemoteRunner {
        cfg,
        pid,
        forward_child,
        local_port,
        url,
    })
}

//------------------------------------------------------------
// 内部：文件校验 / 上传
//------------------------------------------------------------
async fn ensure_remote(cfg: &Config) -> Result<()> {
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

    // 再校验
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
