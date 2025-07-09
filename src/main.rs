mod ssh;

use anyhow::Result;
use ssh::{LocalSource, download_url, file_exists, scp, ssh_run};
use std::io::{self, Write};

pub fn check_s5cmd() -> Result<()> {
    if file_exists("~/.local/bin/s5cmd")? {
        let result = ssh_run("~/.local/bin/s5cmd version")?;
        println!("s5cmd version: {}", result.stdout.trim());
        return Ok(());
    }
    print!("s5cmd not found, downloading...");
    io::stdout().flush().unwrap();
    let bin = download_url(
        "https://github.com/peak/s5cmd/releases/download/v2.3.0/s5cmd_2.3.0_Linux-64bit.tar.gz",
    )?;

    scp(LocalSource::Bytes(bin), "~/s5cmd.tar.gz")?;
    ssh_run("mkdir -p ~/.local/bin")?;
    ssh_run("tar -xzf ~/s5cmd.tar.gz -C ~/.local/bin s5cmd")?;
    ssh_run("chmod +x ~/.local/bin/s5cmd")?;
    ssh_run("rm ~/s5cmd.tar.gz")?;
    if file_exists("~/.local/bin/s5cmd")? {
        println!("Finished.");
        let result = ssh_run("~/.local/bin/s5cmd version")?;
        println!("s5cmd version: {}", result.stdout.trim());
        return Ok(());
    } else {
        Err(anyhow::anyhow!("s5cmd installation failed"))
    }
}

fn main() -> Result<()> {
    check_s5cmd()?;
    Ok(())
}
