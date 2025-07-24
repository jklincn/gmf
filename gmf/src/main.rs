mod config;
mod remote;
use anyhow::Result;
use clap::Parser;
use r2;
use remote::start_remote;
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
    let s3_config = r2::get_config(Some((
        config.endpoint.as_ref(),
        config.access_key_id.as_ref(),
        config.secret_access_key.as_ref(),
    )))
    .await?;

    // 创建 Bucket
    r2::create_bucket(&s3_config).await?;

    // 主逻辑
    let logic_result: Result<()> = async {
        remote.setup(&filepath).await?;
        remote.start().await?;
        Ok(())
    }
    .await;

    if let Err(e) = remote.shutdown().await {
        eprintln!("清理 gmf-remote 时发生错误: {}", e);
    }

    // 清理 Bucket
    if let Err(e) = r2::delete_bucket(&s3_config).await {
        eprintln!("删除 Bucket 时发生错误: {}", e);
    }

    logic_result?;

    Ok(())
}
