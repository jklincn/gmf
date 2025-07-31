use crate::config::Config;
use crate::file::{ChunkResult, GMFFile, GmfSession};
use crate::ssh::{self, CallResult, ExecutionMode};
use anyhow::{Context, Result, anyhow, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{debug, info};
use reqwest_eventsource::{Event, EventSource};
use russh::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

// pub struct TaskContext {
//     progress_bar: AllProgressBar,
//     session: GmfSession,
// }

// impl TaskContext {
//     pub fn new(info: &SetupResponse) -> Result<Self> {
//         let (gmf_file, completed_chunks) =
//             GMFFile::new(&info.filename, info.size, info.total_chunks)?;
//         let progress_bar = AllProgressBar::new(info.total_chunks, completed_chunks)?;
//         let session = GmfSession::new(gmf_file, completed_chunks);

//         Ok(TaskContext {
//             progress_bar,
//             session,
//         })
//     }
// }

pub struct AllProgressBar {
    upload: ProgressBar,
    download: ProgressBar,
    writer: ProgressBar,
}

impl AllProgressBar {
    pub fn new(total_chunks: u64, completed_chunks: u64) -> Result<Self> {
        let mp = MultiProgress::new();
        let upload = mp.add(ProgressBar::new(total_chunks));
        let download = mp.add(ProgressBar::new(total_chunks));
        let writer = mp.add(ProgressBar::new(total_chunks));

        upload.set_style(
            ProgressStyle::default_bar()
                .template("上传进度 [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        download.set_style(
            ProgressStyle::default_bar()
                .template("下载进度 [{bar:40.green/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        writer.set_style(
            ProgressStyle::default_bar()
                .template("写入进度 [{bar:40.green/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        upload.set_position(completed_chunks);
        download.set_position(completed_chunks);
        writer.set_position(completed_chunks);

        Ok(AllProgressBar {
            upload,
            download,
            writer,
        })
    }

    pub fn update_upload(&self) {
        self.upload.inc(1);
    }
    pub fn update_download(&self) {
        self.download.inc(1);
    }
    pub fn update_writer(&self) {
        self.writer.inc(1);
    }
}

pub async fn check_remote(cfg: &Config) -> Result<(ssh::Session, String)> {
    info!("正在尝试连接远程服务器 {}:{}", cfg.host, cfg.port);
    let mut ssh = ssh::Session::connect(cfg).await?;
    info!("远程服务器连接成功");

    const REMOTE_BIN_DIR: &str = "$HOME/.local/bin";
    const REMOTE_PATH: &str = "$HOME/.local/bin/gmf-remote";

    // 检查远程程序 SHA256 的命令
    let check_sha_cmd = format!(
        r#"
        if [ -f "{}" ]; then
            sha256sum "{}" | cut -d' ' -f1
        else
            echo ""
        fi
    "#,
        REMOTE_PATH, REMOTE_PATH
    );

    let needs_install = match ssh.call_once(&check_sha_cmd).await {
        Ok((0, remote_sha)) => {
            // 检查 SHA256 是否匹配
            remote_sha.trim() != GMF_REMOTE_SHA256
        }
        _ => {
            // 任何错误（如命令不存在）都视为需要安装
            true
        }
    };

    // 如果需要，则进行安装/更新
    if needs_install {
        info!("正在安装 gmf-remote 至远程目录: {}", REMOTE_BIN_DIR);

        // 创建远程目录
        ssh.call_once(&format!("mkdir -p {}", REMOTE_BIN_DIR))
            .await?;

        // 上传并解压
        ssh.untar_from_memory(GMF_REMOTE_TAR_GZ, REMOTE_BIN_DIR)
            .await?;

        info!("gmf-remote 安装成功");
    }

    Ok((ssh, REMOTE_PATH.to_string()))
}

pub struct InteractiveSession {
    channel: Channel<client::Msg>,
    ssh_session: ssh::Session,
}

impl InteractiveSession {
    /// 启动远程程序并创建一个新的交互式会话。
    pub async fn start(config: &Config) -> Result<Self> {
        // 检查远程环境并获取 SSH 会话和远程程序路径
        let (mut ssh_session, remote_path) = check_remote(config).await?;

        info!("正在启动 gmf-remote...");
        let command = format!(
            "ENDPOINT='{}' ACCESS_KEY_ID='{}' SECRET_ACCESS_KEY='{}' {}",
            config.endpoint, config.access_key_id, config.secret_access_key, remote_path
        );
        // 以交互模式调用远程程序
        let channel = match ssh_session
            .call(&command, ssh::ExecutionMode::Interactive)
            .await?
        {
            ssh::CallResult::Interactive(channel) => {
                info!("gmf-remote 已成功启动");
                channel
            }
            ssh::CallResult::Once((code, out)) => {
                // 如果程序立即退出，说明启动失败，这是一个致命错误。
                return Err(anyhow!(
                    "尝试以交互模式启动 gmf-remote 失败，程序立即退出。退出码: {}, 输出: {}",
                    code,
                    out
                ));
            }
        };
        Ok(Self {
            channel,
            ssh_session,
        })
    }

    /// 向远程程序发送一条指令，并自动添加换行符。
    pub async fn send_command(&mut self, command: &str) -> Result<()> {
        info!("-> 发送: {}", command);
        let data_to_send = format!("{}\n", command);
        self.channel.data(data_to_send.as_bytes()).await?;
        Ok(())
    }

    /// 等待并接收远程程序的响应，直到满足某个条件（例如看到特定的提示符）。
    ///
    /// # Arguments
    /// * `read_timeout` - 等待响应的总超时时间。
    /// * `end_of_response_marker` - 一个字符串，当在输出中看到它时，就认为响应结束了。
    ///
    /// # Returns
    /// 返回从远程接收到的所有数据，直到标记出现。
    pub async fn receive_response(
        &mut self,
        read_timeout: Duration,
        end_of_response_marker: &str,
    ) -> Result<String> {
        info!("<- 接收 (等待标记: '{}')", end_of_response_marker);
        let mut buffer = Vec::new();

        // 使用 tokio::time::timeout 来防止无限等待
        let processing_fut = async {
            loop {
                match self.channel.wait().await {
                    Some(ChannelMsg::Data { data }) => {
                        buffer.extend_from_slice(&data);
                        // 检查当前缓冲区的内容是否包含结束标记
                        if String::from_utf8_lossy(&buffer).contains(end_of_response_marker) {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        return Err(anyhow!("远程程序意外退出，退出码: {}", exit_status));
                    }
                    None => {
                        return Err(anyhow!("通道在预期响应完成前被关闭"));
                    }
                    _ => {} // 忽略其他消息
                }
            }
            Ok(String::from_utf8(buffer)?)
        };

        match timeout(read_timeout, processing_fut).await {
            Ok(Ok(output)) => {
                info!("<- 接收完成");
                Ok(output)
            }
            Ok(Err(e)) => Err(e), // 内部逻辑错误 (如退出或通道关闭)
            Err(_) => Err(anyhow!("等待响应超时 (超过 {:?})", read_timeout)), // 超时错误
        }
    }

    /// 发送退出指令，并等待程序结束。
    pub async fn shutdown(mut self) -> Result<()> {
        info!("正在关闭会话...");

        // 发送 EOF
        self.channel.eof().await?;

        // 等待远程程序退出
        loop {
            match self.channel.wait().await {
                Some(ChannelMsg::ExitStatus { .. }) | None => break,
                _ => {}
            }
        }

        // 关闭 SSH 连接
        self.ssh_session.close().await?;
        info!("会话已成功关闭。");
        Ok(())
    }
}
