use crate::config::{Config, load_or_create_config};
use anyhow::{Context, Result, anyhow};
use dunce::canonicalize;
use serde::{Deserialize, Serialize};
use shlex::try_quote;
use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

pub struct CommandOutput {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// 分块信息结构体
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChunkInfo {
    pub id: u32,                // 分块序号（从 1 开始）
    pub filename: String,       // 远程最终文件名
    pub passphrase_b64: String, // Base64-encoded 随机口令
    pub sha256: String,         // 加密后文件哈希
    pub size: u64,              // 加密后大小（字节）
}

/// 整个文件的清单
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ManifestFile {
    pub filename: OsString,
    pub total_size: u64,
    pub chunk_size: u64,
    pub total_chunks: u32,

    // —— 加密参数 —— //
    pub cipher: String, // 例如 "AES-256-CBC"
    pub kdf: String,    // 例如 "PBKDF2-SHA256"
    pub kdf_iter: u32,  // 迭代次数
    pub salt_len: u32,  // OpenSSL enc 固定 8 字节

    pub chunks: Vec<ChunkInfo>,
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

        // 私钥支持
        if let Some(key) = &self.config.private_key_path {
            if !key.is_empty() && Path::new(key).exists() {
                cmd.arg("-i").arg(key);
            }
        }

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

/// 分块并加密文件函数，支持更好的错误处理和性能优化
/// 返回清单文件和远程临时工作目录的路径
pub fn encrypt_and_split<P: AsRef<Path>>(path: P) -> Result<(ManifestFile, String)> {
    // 0. 简单的路径安全检查
    let path =
        canonicalize(&path).map_err(|e| anyhow!("无法解析路径 {:?}: {}", path.as_ref(), e))?;

    // todo: 文件夹要打包
    if !path.is_file() {
        return Err(anyhow!("不是普通文件: {}", path.display()));
    }

    // 1. 远程临时工作目录
    let work_dir = {
        let mktemp = ssh_run("mktemp -d")?;
        if mktemp.status_code != Some(0) {
            return Err(anyhow!("创建临时目录失败: {}", mktemp.stderr));
        }
        mktemp.stdout.trim().to_owned()
    };

    // 2. 本地获取文件大小
    let meta = fs::metadata(&path)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("路径里没有文件名: {}", path.display()))?
        .to_owned();

    let file_size = meta.len();

    const CHUNK_SIZE_MB: u64 = 10; // 分块大小，单位 MB
    const CHUNK_SIZE: u64 = CHUNK_SIZE_MB * 1024 * 1024;
    let total_chunks = (file_size + CHUNK_SIZE - 1) / CHUNK_SIZE;
    println!(
        "文件大小: {} bytes, 将分割为 {} 个分块",
        file_size, total_chunks
    );

    // 3. split
    let wd_q: Cow<'_, str> = try_quote(&work_dir)?;
    let path_str = path.to_string_lossy();
    let fp_q: Cow<'_, str> = try_quote(path_str.as_ref())?;

    let split_cmd = format!(
        r#"cd {wd} && FILEPATH={fp} && split -b {chunk_size}M -d "$FILEPATH" chunk_"#,
        wd = wd_q,
        fp = fp_q,
        chunk_size = CHUNK_SIZE_MB,
    );
    let split = ssh_run(&split_cmd)?;
    if split.status_code != Some(0) {
        let _ = ssh_run(&format!("rm -rf {work_dir}"));
        return Err(anyhow::anyhow!("文件分割失败: {}", split.stderr));
    }

    // 4. Bash 批处理脚本
    let script = format!(
        r#"#!/bin/bash
echo "[DEBUG] running on $(hostname) as $(whoami)" >&2
set -euxo pipefail
trap 'echo "[ERR] line:$LINENO cmd:$BASH_COMMAND" >&2' ERR

cd "{wd}"

chunk_count=0
for chunk_file in chunk_*; do
  [[ -f "$chunk_file" ]] || continue

  # *** 生成 32B 随机口令，Base64 方便 manifest 传输
  PASSWORD=$(openssl rand -base64 32)

  (( chunk_count += 1 ))

  # *** 强化：PBKDF2 + SHA256 + 200k 迭代，仍用 AES-256-CBC
  enc_file="${{chunk_file}}.enc"
  openssl enc -aes-256-cbc -salt -pbkdf2 -iter 200000 -md sha256 \
    -in "$chunk_file" -out "$enc_file" -pass pass:"$PASSWORD"

  # *** SHA-256 摘要（替代 MD5）
  sha256=$(openssl dgst -sha256 -r "$enc_file" | awk '{{print $1}}')

  size=$(stat -c%s "$enc_file")
  final_name=$(printf "gmf_chunk_%03d.bin" "$chunk_count")
  mv "$enc_file" "$final_name"
  rm -f "$chunk_file"

  # 输出：id|filename|password|sha256|size
  echo "$chunk_count|$final_name|$PASSWORD|$sha256|$size"
done
"#,
        wd = work_dir
    );

    let proc = ssh_run(&script)?;
    if proc.status_code != Some(0) {
        let _ = ssh_run(&format!("rm -rf {}", wd_q)); // wd_q 已经 quote
        return Err(anyhow::anyhow!(
            "批处理失败（exit={}）\nstdout:\n{}\nstderr:\n{}",
            proc.status_code.unwrap_or(-1),
            proc.stdout,
            proc.stderr
        ));
    }

    // 5. 解析脚本输出
    let mut chunks = Vec::new();
    for line in proc.stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() != 5 {
            return Err(anyhow::anyhow!("分块信息格式错误: {}", line));
        }
        chunks.push(ChunkInfo {
            id: parts[0].parse()?,
            filename: parts[1].into(),
            passphrase_b64: parts[2].into(),
            sha256: parts[3].into(),
            size: parts[4].parse()?,
        });
    }
    chunks.sort_by_key(|c| c.id);

    let manifest = ManifestFile {
        filename: file_name,
        total_size: file_size,
        chunk_size: CHUNK_SIZE,
        total_chunks: total_chunks as u32,
        cipher: "AES-256-CBC".into(),
        kdf: "PBKDF2-SHA256".into(),
        kdf_iter: 200_000,
        salt_len: 8,
        chunks,
    };

    println!("✓ 加密并分块完成，临时目录：{work_dir}");
    Ok((manifest, work_dir))
}
