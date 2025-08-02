mod config;
mod file;
mod io_actor;
mod remote;
mod ssh;
mod ui;

use anyhow::Ok;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use config::Config;
use env_logger::Builder;
use env_logger::Env;
use gmf_common::r2;
use log::{error, info};
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
        0 => "warn",
        _ => "info",
    };
    let env = Env::default().default_filter_or(default_log_level);
    Builder::from_env(env)
        .format(|buf, record| writeln!(buf, "{}", record.args()))
        .init();
}

async fn set_r2(cfg: &Config) -> Result<()> {
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
    let args = Args::parse();

    set_log();

    let cfg = ui::run_with_spinner(
        "正在加载配置文件...",
        "✅ 配置文件加载成功",
        config::load_or_create_config(),
    )
    .await?;

    ui::run_with_spinner(
        "正在初始化 R2 客户端...",
        "✅ R2 客户端初始化成功",
        set_r2(&cfg),
    )
    .await?;

    let mut session = remote::InteractiveSession::new(&cfg).await?;

    let result: Result<()> = tokio::select! {
        // 分支 1: 正常执行业务逻辑
        res = async {
            session
                .setup(&args.path, args.chunk_size, args.concurrency)
                .await?;
            session.start().await?;
            Ok(())
        } => {
            res
        },

        // 分支 2: 监听 Ctrl+C 信号
        _ = signal::ctrl_c() => {
            info!("收到 Ctrl+C 信号，正在清理...");
            Ok(())
        }
    };

    if let Err(e) = &result {
        error!("发生错误: {e:#}");
    } else {
        // 给进度条一些时间显示完成状态
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // 2. 无论主逻辑是否成功，都执行清理操作
    if let Err(e) = session.shutdown().await {
        error!("清理过程中发生错误: {e:#}");
    }

    // 清理 Bucket
    if let Err(e) = r2::delete_bucket_with_retry().await {
        error!("清理 Bucket 时发生错误: {e:#}");
    }
    Ok(())
}
