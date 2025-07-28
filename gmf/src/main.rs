mod config;
mod file;
mod r2;
mod remote;

use anyhow::Result;
use clap::Parser;
use env_logger::Builder;
use log::{debug, error, info, trace, warn};
use std::io::Write;
use tokio::signal;
use env_logger::Env;


/// 解析支持单位 (kb, mb, gb) 的字符串为字节数 (usize)
fn parse_chunk_size(s: &str) -> Result<u64, String> {
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
        .map_err(|_| format!("无效的数字部分: '{}'", num_str))?;

    let bytes = num
        .checked_mul(multiplier)
        .ok_or_else(|| "计算出的数值太大，导致溢出 (超过 u64::MAX)".to_string())?;

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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let default_log_level = match args.verbose {
        0 => "info",
        _ => "debug",
    };
    let env = Env::default().default_filter_or(default_log_level);
    Builder::from_env(env)
        .format(|buf, record| {
            // 只输出级别和消息，不带时间戳
            writeln!(buf, "{}", record.args())
        })
        .init();

    let config = config::load_or_create_config()?;
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
        // BUG: 远程服务未正确关闭
        _ = signal::ctrl_c() => {
            warn!("\n接收到 Ctrl+C 信号，开始清理工作");
            Ok(())
        }
    };

    if let Err(e) = remote.shutdown().await {
        error!("清理 gmf-remote 时发生错误: {:#}", e);
    }
    
    // 清理 Bucket
    if let Err(e) = r2::delete_bucket().await {
        error!("删除 Bucket 时发生错误: {:#}", e);
    }

    logic_result?;

    Ok(())
}
