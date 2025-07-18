mod config;
mod ssh;

use anyhow::Result;
use clap::Parser;
use chunk::{decrypt_and_merge, manifest_from_str};
use ssh::{run_remote, ssh_connect};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    path: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let filepath = args.path;
    ssh_connect()?;
    let result = run_remote(&filepath)?;
    let manifest = manifest_from_str(&result.stdout)?;
    decrypt_and_merge(&manifest,"./decrypted_output")?;

    Ok(())
}
