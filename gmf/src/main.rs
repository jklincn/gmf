mod args;
mod client;
mod comm;
mod config;
mod file;
mod ssh;
mod ui;

use anyhow::Result;
use args::{Command, GetArgs};

use gmf_common::consts::CHUNK_SIZE;
use gmf_common::r2;
use tokio::try_join;

use crate::config::init_r2;

async fn real_main(args: GetArgs) -> Result<()> {
    ui::init_global_logger(args.verbose)?;

    ui::log_info("正在连接...");
    let ((), mut client) = try_join!(init_r2(), client::GMFClient::new(args))?;
    client.spawn_dispatcher();
    let result: Result<()> = tokio::select! {
        // 正常执行业务逻辑
        res = async {
            client.wait_ready().await?;
            client.setup().await?;
            client.start().await?;
            Ok(())
        } => res,

        // 捕捉 Ctrl+C
        _ = tokio::signal::ctrl_c() => {
            ui::abandon_download();
            ui::log_info("正在中断任务...");
            Ok(())
        }
    };

    if let Err(e) = client.shutdown().await {
        ui::log_error(&format!("清理 session 错误: {e:#}"));
    }

    if let Err(e) = r2::delete_bucket().await {
        ui::log_error(&format!("清理 Bucket 时发生错误: {e:#}"));
    }

    result
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = args::get_cli_args();

    match args.command {
        // gmf config
        Command::Config => {
            if let Err(e) = config::reset_config() {
                ui::log_error(&format!("{e:#}"));
            }
        }
        // gmf get <path>
        Command::Get { path, verbose } => {
            let get_args = GetArgs {
                path,
                chunk_size: CHUNK_SIZE,
                verbose,
            };

            if let Err(e) = real_main(get_args).await {
                ui::log_error(&format!("{e:#}"));
            }
        }
    }

    Ok(())
}
