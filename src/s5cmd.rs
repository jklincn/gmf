use crate::config::load_or_create_config;
use crate::ssh::{LocalSource, file_exists, scp, ssh_run};
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use std::io::{self, Write};

/// 下载 URL 内容
fn download_url(url: &str) -> Result<Vec<u8>> {
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

pub fn check_s5cmd() -> Result<()> {
    if file_exists("~/.local/bin/s5cmd")? {
        let result = ssh_run("~/.local/bin/s5cmd version")?;
        println!("s5cmd version: {}", result.stdout.trim());
        return Ok(());
    }
    print!("s5cmd not found, downloading...");
    io::stdout().flush().unwrap();
    let bin = download_url(
        "https://github.com/peak/s5cmd/releases/download/v2.3.0/s5cmd_2.3.0_Linux-64bit.tar.gz",
    )?;

    scp(LocalSource::Bytes(bin), "~/s5cmd.tar.gz")?;
    ssh_run("mkdir -p ~/.local/bin")?;
    ssh_run("tar -xzf ~/s5cmd.tar.gz -C ~/.local/bin s5cmd")?;
    ssh_run("chmod +x ~/.local/bin/s5cmd")?;
    ssh_run("rm ~/s5cmd.tar.gz")?;
    if file_exists("~/.local/bin/s5cmd")? {
        println!("Finished.");
        let result = ssh_run("~/.local/bin/s5cmd version")?;
        println!("s5cmd version: {}", result.stdout.trim());
        return Ok(());
    } else {
        Err(anyhow::anyhow!("s5cmd installation failed"))
    }
}

/// 执行 s5cmd 命令，使用配置文件中的 AWS 凭证
fn run_s5cmd(args: &str) -> Result<crate::ssh::CommandOutput> {
    let config = load_or_create_config()?;

    let access_key_id = config
        .access_key_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("配置文件中未找到 access_key_id"))?;

    let secret_access_key = config
        .secret_access_key
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("配置文件中未找到 secret_access_key"))?;

    let endpoint = config
        .endpoint
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("配置文件中未找到 endpoint"))?;

    let command = format!(
        "AWS_ACCESS_KEY_ID={} AWS_SECRET_ACCESS_KEY={} AWS_REGION='auto' ~/.local/bin/s5cmd --endpoint-url {} {}",
        access_key_id, secret_access_key, endpoint, args
    );

    ssh_run(&command)
}

/// 列出 S3 存储桶或目录内容
pub fn s5cmd_ls(path: &str) -> Result<String> {
    let result = run_s5cmd(&format!("ls {}", path))?;

    match result.status_code {
        Some(0) => Ok(result.stdout),
        Some(1) => {
            if result.stderr.contains("NoSuchBucket") || result.stderr.contains("does not exist") {
                Err(anyhow::anyhow!("存储桶或路径不存在: {}", path))
            } else if result.stderr.contains("AccessDenied") {
                Err(anyhow::anyhow!("访问被拒绝，请检查访问凭证和权限"))
            } else {
                Err(anyhow::anyhow!("列出目录失败: {}", result.stderr.trim()))
            }
        }
        Some(code) => Err(anyhow::anyhow!(
            "s5cmd ls 命令失败 (退出码: {}): {}",
            code,
            result.stderr.trim()
        )),
        None => Err(anyhow::anyhow!("s5cmd ls 进程被信号终止")),
    }
}

/// 上传文件到 S3
pub fn s5cmd_cp(local_path: &str, s3_path: &str) -> Result<String> {
    let result = run_s5cmd(&format!("cp {} s3://{}", local_path, s3_path))?;

    match result.status_code {
        Some(0) => Ok(result.stdout),
        Some(1) => {
            if result.stderr.contains("NoSuchBucket") {
                Err(anyhow::anyhow!("目标存储桶不存在: {}", s3_path))
            } else if result.stderr.contains("AccessDenied") {
                Err(anyhow::anyhow!("访问被拒绝，请检查上传权限"))
            } else if result.stderr.contains("no such file") {
                Err(anyhow::anyhow!("本地文件不存在: {}", local_path))
            } else {
                Err(anyhow::anyhow!("上传文件失败: {}", result.stderr.trim()))
            }
        }
        Some(code) => Err(anyhow::anyhow!(
            "s5cmd cp 命令失败 (退出码: {}): {}",
            code,
            result.stderr.trim()
        )),
        None => Err(anyhow::anyhow!("s5cmd cp 进程被信号终止")),
    }
}

/// 删除 S3 文件
pub fn s5cmd_rm(s3_path: &str) -> Result<String> {
    let result = run_s5cmd(&format!("rm s3://{}", s3_path))?;

    match result.status_code {
        Some(0) => Ok(result.stdout),
        Some(1) => {
            if result.stderr.contains("NoSuchKey") || result.stderr.contains("does not exist") {
                Err(anyhow::anyhow!("要删除的文件不存在: {}", s3_path))
            } else if result.stderr.contains("NoSuchBucket") {
                Err(anyhow::anyhow!("存储桶不存在: {}", s3_path))
            } else if result.stderr.contains("AccessDenied") {
                Err(anyhow::anyhow!("访问被拒绝，请检查删除权限"))
            } else {
                Err(anyhow::anyhow!("删除文件失败: {}", result.stderr.trim()))
            }
        }
        Some(code) => Err(anyhow::anyhow!(
            "s5cmd rm 命令失败 (退出码: {}): {}",
            code,
            result.stderr.trim()
        )),
        None => Err(anyhow::anyhow!("s5cmd rm 进程被信号终止")),
    }
}

/// 创建 S3 存储桶
pub fn s5cmd_mb(bucket_name: &str) -> Result<String> {
    let result = run_s5cmd(&format!("mb s3://{}", bucket_name))?;

    match result.status_code {
        Some(0) => Ok(result.stdout),
        Some(1) => {
            if result.stderr.contains("BucketAlreadyExists")
                || result.stderr.contains("already exists")
            {
                Err(anyhow::anyhow!("存储桶已存在: {}", bucket_name))
            } else if result.stderr.contains("AccessDenied") {
                Err(anyhow::anyhow!("访问被拒绝，请检查创建存储桶的权限"))
            } else if result.stderr.contains("InvalidBucketName") {
                Err(anyhow::anyhow!("存储桶名称无效: {}", bucket_name))
            } else {
                Err(anyhow::anyhow!("创建存储桶失败: {}", result.stderr.trim()))
            }
        }
        Some(code) => Err(anyhow::anyhow!(
            "s5cmd mb 命令失败 (退出码: {}): {}",
            code,
            result.stderr.trim()
        )),
        None => Err(anyhow::anyhow!("s5cmd mb 进程被信号终止")),
    }
}

/// 删除 S3 存储桶
pub fn s5cmd_rb(bucket_name: &str) -> Result<String> {
    let result = run_s5cmd(&format!("rb s3://{}", bucket_name))?;

    match result.status_code {
        Some(0) => Ok(result.stdout),
        Some(1) => {
            if result.stderr.contains("NoSuchBucket") || result.stderr.contains("does not exist") {
                Err(anyhow::anyhow!("存储桶不存在: {}", bucket_name))
            } else if result.stderr.contains("BucketNotEmpty")
                || result.stderr.contains("not empty")
            {
                Err(anyhow::anyhow!(
                    "存储桶不为空，无法删除: {}。请先清空存储桶中的所有文件",
                    bucket_name
                ))
            } else if result.stderr.contains("AccessDenied") {
                Err(anyhow::anyhow!("访问被拒绝，请检查删除存储桶的权限"))
            } else {
                Err(anyhow::anyhow!("删除存储桶失败: {}", result.stderr.trim()))
            }
        }
        Some(code) => Err(anyhow::anyhow!(
            "s5cmd rb 命令失败 (退出码: {}): {}",
            code,
            result.stderr.trim()
        )),
        None => Err(anyhow::anyhow!("s5cmd rb 进程被信号终止")),
    }
}
