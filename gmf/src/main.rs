mod config;
mod gmf_file;
mod r2;
mod remote;

use anyhow::Result;
use clap::Parser;
use remote::start_remote;

const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10 MiB

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    path: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let filepath = args.path;
    let config = config::load_or_create_config()?;

    let mut remote = start_remote(&config).await?;

    // 主逻辑
    let logic_result: Result<()> = async {
        remote.setup(&filepath, CHUNK_SIZE).await?;
        remote.start().await?;
        Ok(())
    }
    .await;

    if let Err(e) = remote.shutdown().await {
        eprintln!("清理 gmf-remote 时发生错误: {}", e);
    }

    // 清理 Bucket
    if let Err(e) = r2::delete_bucket().await {
        eprintln!("删除 Bucket 时发生错误: {}", e);
    }

    logic_result?;

    Ok(())
}
