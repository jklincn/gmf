mod config;
mod s5cmd;
mod ssh;

use anyhow::Result;
use s5cmd::{check_s5cmd, s5cmd_cp, s5cmd_mb, s5cmd_rb, s5cmd_rm};
use ssh::ssh_connect;

fn main() -> Result<()> {
    ssh_connect()?;
    check_s5cmd()?;
    s5cmd_mb("gmf")?;
    s5cmd_cp("~/rust_prj/gmf/home", "gmf/home")?;
    s5cmd_rm("gmf/home")?;
    s5cmd_rb("gmf")?;
    Ok(())
}
