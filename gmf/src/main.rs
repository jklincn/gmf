mod config;
mod file;
mod remote;

use anyhow::Ok;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use config::Config;
use env_logger::Builder;
use env_logger::Env;
use gmf_common::r2;
use log::{error, warn};
use std::io::Write;
use tokio::signal;

/// 解析支持单位 (kb, mb, gb) 的字符串为字节数 (usize)
fn parse_chunk_size(s: &str) -> Result<u64> {
    let s_lower = s.to_lowercase();

    let (num_str, multiplier): (&str, u64) = if let Some(stripped) = s_lower.strip_suffix("gb") {
        (stripped.trim(), 1024 * 1024 * 1024)
    } else if let Some(stripped) = s_lower.strip_suffix("mb") {
        (stripped.trim(), 1024 * 1024)
    } else if let Some(stripped) = s_lower.strip_suffix("kb") {
        (stripped.trim(), 1024)
    } else {
        (s_lower.as_str(), 1)
    };

    // 解析数字部分
    let num = num_str
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow!("无效的数字部分: '{}'", num_str))?;

    let bytes = num
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("计算出的数值太大，导致溢出 (超过 u64::MAX)"))?;

    Ok(bytes)
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = None
)]
struct Args {
    /// 要上传的文件路径
    path: String,

    /// 设置分块大小。支持单位 (KB, MB, GB) 或纯数字 (字节)。
    /// 示例: 10MB, 256KB, 1gb, 10485760
    #[arg(
        long,
        short = 'c',
        value_name = "SIZE",
        default_value_t = 10 * 1024 * 1024, // 默认值: 10 MiB
        value_parser = parse_chunk_size
    )]
    chunk_size: u64,

    /// 设置并发上传的任务数量
    #[arg(
        long,
        short = 'n',
        value_name = "NUMBER",
        default_value_t = 4 // 默认值: 4
    )]
    concurrency: u64,

    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn set_log() {
    let default_log_level = match Args::parse().verbose {
        0 => "info",
        _ => "debug",
    };
    let env = Env::default().default_filter_or(default_log_level);
    Builder::from_env(env)
        .format(|buf, record| writeln!(buf, "{}", record.args()))
        .init();
}

async fn set_s3(cfg: &Config) -> Result<()> {
    let s3_config = r2::S3Config {
        endpoint: cfg.endpoint.clone(),
        access_key_id: cfg.access_key_id.clone(),
        secret_access_key: cfg.secret_access_key.clone(),
    };
    r2::init_s3_client(Some(s3_config)).await?;
    r2::create_bucket().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    set_log();

    let args = Args::parse();

    let config = config::load_or_create_config()?;

    set_s3(&config).await?;

    let mut remote = remote::start_remote(&config).await?;

    // 主逻辑
    let logic_result: Result<()> = tokio::select! {
        // 分支 1: 正常执行业务逻辑
        res = async {
            remote
                .setup(&args.path, args.chunk_size, args.concurrency)
                .await?;
            remote.start().await?;
            Ok(())
        } => {
            res
        },

        // 分支 2: 监听 Ctrl+C 信号
        _ = signal::ctrl_c() => {
            warn!("\n接收到 Ctrl+C 信号，开始清理工作");
            Ok(())
        }
    };

    // 1. 如果主逻辑出错，先打印错误信息
    if let Err(e) = &logic_result {
        error!("执行失败: {e:#}");
    }

    // 2. 无论主逻辑是否成功，都执行清理操作
    if let Err(e) = remote.shutdown().await {
        error!("清理 gmf-remote 时发生错误: {e:#}");
    }

    // 清理 Bucket
    if let Err(e) = r2::delete_bucket().await {
        error!("删除 Bucket 时发生错误: {e:#}");
    }

    Ok(())
}
