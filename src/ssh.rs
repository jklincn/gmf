use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

const CONFIG_FILE_NAME: &str = "config.toml";

/// SSH 配置
#[derive(Serialize, Deserialize, Debug)]
pub struct SSHConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub private_key_path: Option<String>,
}

pub struct CommandOutput {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// 全局 SSH 配置，首次访问时加载或创建
static SSH_CONFIG: Lazy<SSHConfig> =
    Lazy::new(|| load_or_create_config().expect("初始化 SSH 配置失败"));

/// 全局 ssh 命令路径，首次访问时检测
static SSH_CMD: Lazy<String> = Lazy::new(|| detect_ssh_command().expect("检测 SSH 命令失败"));

/// 检测 SSH 命令是否存在
fn detect_ssh_command() -> Result<String> {
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

/// 加载或创建配置文件
fn load_or_create_config() -> Result<SSHConfig> {
    let path = Path::new(CONFIG_FILE_NAME);
    if path.exists() {
        let content = fs::read_to_string(path).context("读取配置文件失败")?;
        let cfg: SSHConfig = toml::from_str(&content).context("解析配置文件失败")?;
        Ok(cfg)
    } else {
        let default = SSHConfig {
            host: "192.168.1.100".into(),
            port: 22,
            user: "root".into(),
            password: Some("your_password".into()),
            private_key_path: Some("your_private_key_path".into()),
        };
        let header = "# SSH 连接配置\n\
              # host: 目标主机IP或域名\n\
              # port: SSH端口\n\
              # user: 登录用户名\n\
              # password: 登录密码\n\
              # private_key_path: 私钥路径（Windows路径使用单引号包裹）\n\
              # 如果同时设置了 password 和 private_key_path，则优先使用私钥登录\n\n";

        let mut body = toml::to_string_pretty(&default).context("序列化默认配置失败")?;
        body = body.replace(
            "private_key_path = \"your_private_key_path\"",
            "private_key_path = 'your_private_key_path'",
        );
        fs::write(path, format!("{}{}", header, body)).context("写入默认配置失败")?;
        println!(
            "已生成默认配置文件 '{}', 请根据实际情况修改配置信息",
            CONFIG_FILE_NAME
        );
        std::process::exit(0);
    }
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

pub enum LocalSource {
    /// 本地文件路径
    Path(String),
    /// 内存字节数据
    Bytes(Vec<u8>),
}

/// 下载 URL 内容
pub fn download_url(url: &str) -> Result<Vec<u8>> {
    let client = Client::new();
    let resp = client
        .get(url)
        .send()
        .context("下载请求失败")?
        .error_for_status()
        .context("非 2xx 响应")?;
    let bytes = resp.bytes().context("读取响应字节失败")?;
    Ok(bytes.to_vec())
}

/// 使用 scp 将本地源上传到远程主机，返回 stdout 和 stderr
pub fn scp(source: LocalSource, remote_path: &str) -> Result<CommandOutput> {
    let config = &*SSH_CONFIG;
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
    let mut cmd = Command::new("scp");
    cmd.arg("-P").arg(config.port.to_string());
    // 私钥支持
    if let Some(key) = &config.private_key_path {
        if !key.is_empty() && Path::new(key).exists() {
            cmd.arg("-i").arg(key);
        }
    }
    let dest = format!("{}@{}:{}", config.user, config.host, remote_path);
    cmd.arg(local_path).arg(dest);

    execute_command(cmd)
}

/// 通过 SSH 执行命令并捕获输出
pub fn ssh_run(command: &str) -> Result<CommandOutput> {
    let ssh_cmd = &*SSH_CMD;
    let config = &*SSH_CONFIG;
    let target = format!("{}@{}", config.user, config.host);
    let mut cmd = Command::new(ssh_cmd);
    cmd.arg("-p").arg(config.port.to_string());
    if let Some(key) = &config.private_key_path {
        if !key.is_empty() && Path::new(key).exists() {
            cmd.arg("-i").arg(key);
        }
    }
    cmd.arg(target).arg(command);
    execute_command(cmd)
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
