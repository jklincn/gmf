//! src/remote_runner.rs
//! 依赖见 Cargo.toml：tokio="1", reqwest="0.12", anyhow, sha2
use crate::config::{Config, load_or_create_config};
use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::sleep,
};

include!(concat!(env!("OUT_DIR"), "/gmf-remote.rs"));

const TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

//------------------------------------------------------------
// 对外结构体
//------------------------------------------------------------
pub struct RemoteRunner {
    cfg: Config,
    child: Child,
    pid: u32,
    port: u16,
}

impl RemoteRunner {
    pub async fn shutdown(&mut self) -> Result<()> {
        if self.child.id().is_some() {
            self.child.kill().await.ok();
            self.child.wait().await.ok();
        }
        let cmd = format!("kill {}", self.pid);
        ssh_once(&self.cfg, &cmd)
            .await
            .context("远端 gmf-remote 杀掉失败")?;
        Ok(())
    }
}

//------------------------------------------------------------
// 入口：启动远程并返回 Runner
//------------------------------------------------------------
pub async fn start_remote() -> Result<RemoteRunner> {
    let cfg = load_or_create_config()?;
    ensure_remote(&cfg).await?;

    //--------------------------------------------------------
    // 1. ssh 启动 gmf-remote
    //--------------------------------------------------------
    let mut child = {
        let mut cmd = Command::new("ssh");
        add_ssh_args(&mut cmd, &cfg)
            .arg("~/.local/bin/gmf-remote")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        cmd.spawn().context("无法启动 ssh 进程")?
    };

    //--------------------------------------------------------
    // 2. 读取首行端口号
    //--------------------------------------------------------
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("无法获得 ssh stdout"))?;
    let mut reader = BufReader::new(stdout).lines();

    // 1. 读取并解析 PID
    let pid_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("远端未输出 PID"))?;
    println!("收到远端 PID：{}", pid_line);
    let pid: u32 = pid_line.trim().parse().context("PID 解析失败")?;

    // 2. 读取并解析端口号
    let port_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("远端未输出端口号"))?;
    println!("收到远端端口：{}", port_line);
    let port: u16 = port_line.trim().parse().context("端口号解析失败")?;

    println!("gmf-remote (PID {}) 正在监听端口 {}", pid, port);

    //--------------------------------------------------------
    // 3. 本地轮询直到 200
    //--------------------------------------------------------
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .danger_accept_invalid_certs(true)
        .build()?;
    let deadline = Instant::now() + TIMEOUT;

    loop {
        match client
            .get(format!("https://{}:{port}", cfg.host))
            .send()
            .await
        {
            Ok(resp) if resp.status() == 200 => break,
            _ if Instant::now() > deadline => {
                child.kill().await.ok();
                return Err(anyhow!("等待端口 {port} 就绪超时 (> {TIMEOUT:?})"));
            }
            _ => sleep(RETRY_INTERVAL).await,
        }
    }

    Ok(RemoteRunner {
        cfg,
        child,
        pid,
        port,
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
