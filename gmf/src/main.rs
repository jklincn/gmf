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
    r2::create_bucket(&s3_config).await?;
    
    remote.setup(&filepath).await?;
    remote.start().await?;
    remote.shutdown().await?;

    r2::delete_bucket(&s3_config).await?;

    Ok(())
}
