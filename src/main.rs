mod config;
mod core;

use crate::config::prompt_config;
use crate::core::{create_worker, enable_workers_dev};
use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "gmf", version, about = "GMF CLI with interactive config")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run interactive setup (gmf config)
    Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    create_worker().await?;
    enable_workers_dev().await?;

    let cli = Cli::parse();
    match cli.command {
        Commands::Config => {
            prompt_config()?;
        }
    }

    Ok(())
}
