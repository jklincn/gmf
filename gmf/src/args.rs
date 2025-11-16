use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "重置 GMF 的配置文件")]
    Config,
    #[command(about = "从远程服务器下载文件")]
    Get {
        path: String,

        /// 打印详细输出
        #[arg(short, long)]
        verbose: bool,
    },
}

/// Get 参数
#[derive(Debug, Clone)]
pub struct GetArgs {
    pub path: String,
    pub chunk_size: u64,
    pub verbose: bool,
}

pub fn get_cli_args() -> Cli {
    Cli::parse()
}
