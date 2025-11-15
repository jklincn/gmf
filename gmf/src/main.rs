mod comm;
mod config;
mod file;
mod client;
mod ssh;
mod ui;

use anyhow::{Result, anyhow};
use clap::Parser;
use gmf_common::r2;
use tokio::try_join;

use crate::config::init_r2;

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
    /// 要下载的远程文件路径
    path: String,

    /// 分块大小
    #[arg(
        long,
        short = 'c',
        value_name = "SIZE",
        default_value_t = 10 * 1024 * 1024,
        value_parser = parse_chunk_size
    )]
    chunk_size: u64,

    /// 打印详细输出
    #[arg(short, long)]
    verbose: bool,
}

async fn real_main(args: Args) -> Result<()> {
    ui::init_global_logger(args.verbose)?;

    ui::log_info("正在连接...");
    let ((), mut client) = try_join!(init_r2(), client::GMFClient::new(args.verbose),)?;

    let result: Result<()> = tokio::select! {
        // 正常执行业务逻辑
        res = async {
            client.setup(&args.path, args.chunk_size).await?;
            client.start().await?;
            Ok(())
        } => res,

        // 捕捉 Ctrl+C
        _ = tokio::signal::ctrl_c() => {
            ui::abandon_download();
            ui::log_warn("正在中断任务...");
            Ok(())
        }
    };

    if let Err(e) = client.shutdown().await {
        ui::log_error(&format!("清理 session 错误: {e:#}"));
    }

    if let Err(e) = r2::delete_bucket().await {
        ui::log_error(&format!("清理 Bucket 时发生错误: {e:#}"));
    }

    result
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Err(e) = real_main(args).await {
        ui::log_error(&format!("{e:#}"));
    }
    Ok(())
}
