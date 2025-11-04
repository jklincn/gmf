mod config;
mod cf;

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
    /// Push file to Cloudflare (gmf push)
    Push,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Config => {
            config::prompt_config()?;
        }
        Commands::Push => {
            config::load_config()?;
            cf::push().await?;
        }
    }

    Ok(())
}
