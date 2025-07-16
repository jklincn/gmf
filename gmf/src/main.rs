mod config;
// mod local;
mod ssh;

use anyhow::Result;
use clap::Parser;
// use local::decrypt_and_merge_local;

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
    run_remote(&filepath)?;

    // let (manifest, work_dir) = encrypt_and_split(&args.path)?;
    // decrypt_and_merge_local(&manifest, &work_dir)?;
    Ok(())
}
