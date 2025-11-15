use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};

/// 解析支持单位 (kb, mb, gb) 的字符串为字节数 (u64)
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
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "重置 GMF 的配置文件")]
    Config,
    #[command(about = "从远程服务器下载文件")]
    Get {
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
    },
}

/// Get 参数
#[derive(Debug, Clone)]
pub struct GetArgs {
    pub path: String,
    pub chunk_size: u64,
    pub verbose: bool,
}

pub fn get_cli_args() -> Cli {
    Cli::parse()
}
