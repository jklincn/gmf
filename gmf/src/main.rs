mod config;
mod gmf_file;
mod r2;
mod remote;

use anyhow::Result;
use clap::Parser;
use remote::start_remote;

// 1. 定义自定义的解析函数
/// 解析支持单位 (kb, mb, gb) 的字符串为字节数 (usize)
fn parse_chunk_size(s: &str) -> Result<usize, String> {
    let s_lower = s.to_lowercase();
    let (num_str, multiplier) = if let Some(stripped) = s_lower.strip_suffix("gb") {
        (stripped, 1024 * 1024 * 1024)
    } else if let Some(stripped) = s_lower.strip_suffix("mb") {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = s_lower.strip_suffix("kb") {
        (stripped, 1024)
    } else {
        // 如果没有单位，则整个字符串都是数字，乘数为 1
        (s_lower.as_str(), 1)
    };

    // 解析数字部分
    let num = num_str
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("无效的数字部分: '{}'", num_str))?;

    // 计算最终的字节数，并检查溢出
    let bytes = num
        .checked_mul(multiplier)
        .ok_or_else(|| "计算出的数值太大，导致溢出".to_string())?;

    // 将 u64 转换为 usize，这在 32 位系统上是必要的安全检查
    usize::try_from(bytes).map_err(|_| "数值对于当前系统架构过大 (超过 usize)".to_string())
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = "GMF(Get My File): 使用运营商白名单绕开限速获取我的文件"
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
    chunk_size: usize,

    /// 设置并发上传的任务数量
    #[arg(
        long,
        short = 'n',
        value_name = "NUMBER",
        default_value_t = 4 // 默认值: 4
    )]
    concurrency: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 3. 解析参数，现在包含了 chunk_size 和 concurrency
    let args = Args::parse();
    let filepath = args.path;
    let config = config::load_or_create_config()?;

    let mut remote = start_remote(&config).await?;

    // 主逻辑
    let logic_result: Result<()> = async {
        // 4. 使用从 args 中获取的值，而不是 const 常量
        remote
            .setup(&filepath, args.chunk_size, args.concurrency)
            .await?;
        remote.start().await?;
        Ok(())
    }
    .await;

    if let Err(e) = remote.shutdown().await {
        eprintln!("清理 gmf-remote 时发生错误: {e}");
    }

    // 清理 Bucket
    if let Err(e) = r2::delete_bucket().await {
        eprintln!("删除 Bucket 时发生错误: {e}");
    }

    logic_result?;

    Ok(())
}
