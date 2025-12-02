use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "登陆-创建用户配置文件")]
    Login,
    #[command(about = "从远程服务器下载文件")]
    Get {
        path: String,

        /// 更加详细的输出
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
