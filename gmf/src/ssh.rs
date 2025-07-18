use crate::config::{Config, load_or_create_config};
use anyhow::{Context, Result};
use bytes::Bytes;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

include!(concat!(env!("OUT_DIR"), "/gmf-remote.rs"));

pub struct CommandOutput {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// SSH ControlMaster 管理器
pub struct SSHManager {
    control_path: PathBuf,
    config: Config,
    ssh_cmd: String,
    master_started: bool,
}

pub enum LocalSource {
    /// 本地文件路径
    Path(String),
    /// 内存字节数据
    Bytes(Vec<u8>),
}

impl SSHManager {
    fn new() -> Result<Self> {
        let ssh_cmd = check_ssh_command()?;
        let config = load_or_create_config()?;
        let control_path = std::env::temp_dir().join(format!(
            "ssh_ctrl_{}_{}_{}",
            config.user, config.host, config.port
        ));

        let mut manager = SSHManager {
            control_path,
            config,
            ssh_cmd,
            master_started: false,
        };

        // 在new的时候就建立并测试连接
        let target = format!("{}@{}", manager.config.user, manager.config.host);
        let mut cmd = Command::new(&manager.ssh_cmd);
        cmd.args([
            "-M", // Master mode
            "-S",
            manager.control_path.to_str().unwrap(), // Control socket path
            "-f",                                   // 后台运行
            "-N",                                   // 不执行远程命令
            "-o",
            "ControlPersist=10m", // 保持连接10分钟
            "-p",
            &manager.config.port.to_string(),
        ]);

        // 私钥支持
        if let Some(key) = &manager.config.private_key_path {
            if !key.is_empty() && Path::new(key).exists() {
                cmd.arg("-i").arg(key);
            }
        }

        cmd.arg(target);

        let output = cmd.output().context("启动 SSH master 连接失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("SSH master 连接失败: {}", stderr));
        }

        manager.master_started = true;
        Ok(manager)
    }

    fn ensure_master_connection(&self) -> Result<()> {
        if !self.master_started {
            return Err(anyhow::anyhow!("SSH master 连接未建立"));
        }

        // 使用 ssh -O check 来检查连接状态
        let target = format!("{}@{}", self.config.user, self.config.host);
        let mut cmd = Command::new(&self.ssh_cmd);
        cmd.args([
            "-S",
            self.control_path.to_str().unwrap(),
            "-p",
            &self.config.port.to_string(),
            "-O",
            "check",
            &target,
        ]);

        let output = cmd.output().context("检查 SSH master 连接状态失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("SSH master 连接已断开: {}", stderr));
        }

        Ok(())
    }

    fn ssh(&mut self, command: &str) -> Result<CommandOutput> {
        self.ensure_master_connection()?;

        let target = format!("{}@{}", self.config.user, self.config.host);
        let mut cmd = Command::new(&self.ssh_cmd);
        cmd.args([
            "-S",
            self.control_path.to_str().unwrap(), // 使用已有的master连接
            "-o",
            "ControlMaster=auto",
            "-p",
            &self.config.port.to_string(),
        ]);

        cmd.arg(target).arg(command);
        execute_command(cmd)
    }

    fn scp(&mut self, local_path: &str, remote_path: &str) -> Result<CommandOutput> {
        self.ensure_master_connection()?;

        let dest = format!("{}@{}:{}", self.config.user, self.config.host, remote_path);
        let mut cmd = Command::new("scp");
        cmd.args([
            "-o",
            &format!("ControlPath={}", self.control_path.to_str().unwrap()),
            "-o",
            "ControlMaster=auto",
            "-P",
            &self.config.port.to_string(),
        ]);

        cmd.arg(local_path).arg(dest);
        execute_command(cmd)
    }
}

impl Drop for SSHManager {
    fn drop(&mut self) {
        if self.master_started {
            let target = format!("{}@{}", self.config.user, self.config.host);
            let _ = Command::new(&self.ssh_cmd)
                .args([
                    "-S",
                    self.control_path.to_str().unwrap(),
                    "-p",
                    &self.config.port.to_string(), // 添加port参数
                    "-O",
                    "exit", // 退出master
                    &target,
                ])
                .output();
        }
    }
}

/// 全局 SSH 管理器
static SSH_MANAGER: std::sync::OnceLock<std::sync::Mutex<SSHManager>> = std::sync::OnceLock::new();

/// 初始化并检查SSH连接
pub fn ssh_connect() -> Result<()> {
    let manager = SSHManager::new()?;

    // 初始化全局管理器
    SSH_MANAGER
        .set(std::sync::Mutex::new(manager))
        .map_err(|_| anyhow::anyhow!("SSH管理器已经初始化"))?;

    Ok(())
}

/// 检测 SSH 命令是否存在
fn check_ssh_command() -> Result<String> {
    let cmds = if cfg!(target_os = "windows") {
        vec!["ssh.exe", "ssh"]
    } else {
        vec!["ssh"]
    };
    for cmd in cmds {
        let output = Command::new(cmd).arg("-V").output();
        if let Ok(out) = output {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.to_lowercase().contains("openssh") {
                println!("ssh version: {}", stderr.trim());
                return Ok(cmd.to_string());
            }
        }
    }
    Err(anyhow::anyhow!(
        "未找到可用的 SSH 命令，请安装 OpenSSH 客户端"
    ))
}

/// 执行命令并捕获输出
fn execute_command(mut cmd: Command) -> Result<CommandOutput> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().context("启动子进程失败")?;
    let output = child.wait_with_output().context("等待子进程结束失败")?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(CommandOutput {
        status_code: output.status.code(),
        stdout,
        stderr,
    })
}

/// 使用 scp 将本地源上传到远程主机，返回 stdout 和 stderr
pub fn scp(source: LocalSource, remote_path: &str) -> Result<CommandOutput> {
    let manager_mutex = SSH_MANAGER
        .get()
        .ok_or_else(|| anyhow::anyhow!("SSH连接未初始化，请先调用 check_ssh_connection()"))?;

    let mut _tmp_holder: Option<NamedTempFile> = None;
    let local_path = match source {
        LocalSource::Path(p) => p,
        LocalSource::Bytes(data) => {
            let mut tmp = NamedTempFile::new().context("创建临时文件失败")?;
            tmp.write_all(&data).context("写入临时文件失败")?;
            let path = tmp.path().to_string_lossy().into_owned();
            _tmp_holder = Some(tmp);
            path
        }
    };

    let mut manager = manager_mutex.lock().unwrap();
    manager.scp(&local_path, remote_path)
}

/// 通过 SSH 执行命令并捕获输出
pub fn ssh_run(command: &str) -> Result<CommandOutput> {
    let manager_mutex = SSH_MANAGER
        .get()
        .ok_or_else(|| anyhow::anyhow!("SSH连接未初始化，请先调用 check_ssh_connection()"))?;
    let mut manager = manager_mutex.lock().unwrap();
    manager.ssh(command)
}

/// 检查远程文件是否存在，返回 bool
pub fn file_exists(path: &str) -> Result<bool> {
    let result = ssh_run(&format!("test -e {}", path))?;
    match result.status_code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        Some(other_code) => Err(anyhow::anyhow!(
            "检查文件时发生意外错误 (退出码: {}): {}",
            other_code,
            result.stderr
        )),
        None => Err(anyhow::anyhow!("检查文件的远程进程被信号终止")),
    }
}

pub fn check_remote() -> Result<()> {
    if file_exists("~/.local/bin/gmf-remote")? {
        let proc = ssh_run("openssl dgst -sha256 -r ~/.local/bin/gmf-remote | awk '{{print $1}}'")?;
        if proc.stdout.trim() == REMOTE_ELF_SHA256 {
            return Ok(());
        } else {
            println!("远程 ELF SHA256 不匹配，重新安装");
        }
    }

    let remote_data: Bytes = Bytes::from_static(REMOTE_ELF);
    let src = LocalSource::Bytes(remote_data.to_vec());
    scp(src, "~/gmf-remote.gz")?;
    ssh_run("mkdir -p ~/.local/bin")?;
    ssh_run("gunzip -c ~/gmf-remote.gz > ~/.local/bin/gmf-remote")?;
    let proc = ssh_run("openssl dgst -sha256 -r ~/.local/bin/gmf-remote | awk '{{print $1}}'")?;
    if proc.stdout.trim() != REMOTE_ELF_SHA256 {
        return Err(anyhow::anyhow!(
            "远程 ELF 校验失败，期望: {}, 实际: {}",
            REMOTE_ELF_SHA256,
            proc.stdout.trim()
        ));
    }
    ssh_run("chmod +x ~/.local/bin/gmf-remote")?;
    ssh_run("rm ~/gmf-remote.gz")?;
    Ok(())
}

pub fn run_remote(path: &str) -> Result<CommandOutput> {
    check_remote()?;
    let result = ssh_run(&format!("~/.local/bin/gmf-remote '{path}'"))?;
    if result.status_code != Some(0) {
        return Err(anyhow::anyhow!(
            "远程执行失败 (退出码: {}): {}",
            result.status_code.unwrap_or(-1),
            result.stderr
        ));
    }
    println!("{}", result.stdout.trim());
    Ok(result)
}
