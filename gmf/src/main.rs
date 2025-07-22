mod config;
mod remote;
use anyhow::Result;
use clap::Parser;
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

    let mut remote = start_remote().await?;
    remote.setup(&filepath).await?;
    remote.start().await?;
    remote.shutdown().await?;

    Ok(())
}
