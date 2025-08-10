mod config;
mod file;
mod io_actor;
mod remote;
mod ssh;
mod ui;

use anyhow::{Result, anyhow};
use clap::Parser;
use config::{Config, ConfigError};
use gmf_common::r2;

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
    ui::init_global_logger(args.verbose)?;

    let cfg_result = ui::run_with_spinner(
        "正在加载配置文件...",
        "✅ 配置文件加载成功",
        config::load_or_create_config(),
    )
    .await;

    let cfg = match cfg_result {
        Ok(config) => config,
        Err(e) => {
            if e.downcast_ref::<ConfigError>().is_some() {
                std::process::exit(0);
            } else {
                return Err(e);
            }
        }
    };

    ui::run_with_spinner(
        "正在初始化 R2 客户端...",
        "✅ R2 客户端初始化成功",
        set_r2(&cfg),
    )
    .await?;

    let mut session = remote::InteractiveSession::new(&cfg, args.verbose).await?;

    let result: Result<()> = tokio::select! {
        // 分支 1: 正常执行业务逻辑
        res = async {
            session
                .setup(&args.path, args.chunk_size)
                .await?;
            session.start().await?;
            Ok(())
        } => {
            res
        },

        // 分支 2: 监听 Ctrl+C 信号
        _ = tokio::signal::ctrl_c() => {
            ui::abandon_download();
            ui::log_warn("⛔ 收到 Ctrl+C 信号，正在清理...请不要再次输入 Ctrl+C");
            Ok(())
        }
    };

    if let Err(e) = &result {
        ui::log_error(&format!("发生错误: {e:#}"));
    }

    // 无论主逻辑是否成功，都执行清理操作
    if let Err(e) = session.shutdown().await {
        ui::log_error(&format!("清理过程中发生错误: {e:#}"));
    }

    // 清理 Bucket
    if let Err(e) = r2::delete_bucket_with_retry().await {
        ui::log_error(&format!("清理 Bucket 时发生错误: {e:#}"));
    }

    Ok(())
}
