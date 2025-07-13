mod config;
mod local;
mod remote;
mod s5cmd;

use anyhow::Result;
use clap::Parser;
use local::decrypt_and_merge_local;
use remote::{encrypt_and_split, ssh_connect};
use s5cmd::{check_s5cmd, s5cmd_cp, s5cmd_mb, s5cmd_rb, s5cmd_rm};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(value_parser = expand_tilde)]
    path: PathBuf,
}

fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    Ok(match shellexpand::tilde(raw).into_owned() {
        s if s.is_empty() => return Err("路径不能为空".into()),
        s => PathBuf::from(s),
    })
}

fn main() -> Result<()> {
    let args = Args::parse();
    ssh_connect()?;
    // check_s5cmd()?;
    // s5cmd_mb("gmf")?;
    // s5cmd_cp("~/rust_prj/gmf/home", "gmf/home")?;
    // s5cmd_rm("gmf/home")?;
    // s5cmd_rb("gmf")?;
    let (manifest, work_dir) = encrypt_and_split(&args.path)?;
    print!("清单文件: {:?}\n", manifest);
    print!("保存目录: {}\n", work_dir);
    decrypt_and_merge_local(&manifest, &work_dir)?;
    Ok(())
}
