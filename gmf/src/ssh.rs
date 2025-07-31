use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use anyhow::{Context, Result, anyhow};
use gmf_common::utils::format_size;
use log::info;
use russh::keys::*;
use russh::*;
use russh_sftp::{client::SftpSession, protocol::OpenFlags};
use tokio::io::AsyncWriteExt;

struct ClientHandle {}

impl client::Handler for ClientHandle {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Once,
    Interactive,
}

/// 封装 `call` 方法的两种可能返回结果
#[derive(Debug)]
pub enum CallResult {
    Once((u32, String)),
    Interactive(Channel<client::Msg>),
}

pub struct Session {
    session: client::Handle<ClientHandle>,
}

impl Session {
    pub async fn connect(cfg: &Config) -> Result<Self> {
        let key_pair = load_secret_key(&cfg.private_key_path, None)?;

        let config = client::Config {
            inactivity_timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        };

        let config = Arc::new(config);
        let sh = ClientHandle {};

        let addr = (cfg.host.as_str(), cfg.port);
        let mut session = client::connect(config, addr, sh).await?;

        let auth_res = session
            .authenticate_publickey(
                cfg.user.as_str(),
                PrivateKeyWithHashAlg::new(
                    Arc::new(key_pair),
                    session.best_supported_rsa_hash().await?.flatten(),
                ),
            )
            .await?;

        if !auth_res.success() {
            anyhow::bail!("公钥认证失败");
        }

        Ok(Self { session })
    }

    pub async fn call(&mut self, command: &str, mode: ExecutionMode) -> Result<CallResult> {
        let mut channel = self.session.channel_open_session().await?;
        channel.exec(true, command).await?;
        match mode {
            ExecutionMode::Once => {
                let mut code = None;
                let mut output_buffer = Vec::new();

                loop {
                    let Some(msg) = channel.wait().await else {
                        break;
                    };
                    match msg {
                        ChannelMsg::Data { ref data } => {
                            // 收集 stdout
                            output_buffer.extend_from_slice(data);
                        }
                        // 捕获 stderr
                        ChannelMsg::ExtendedData { ext: 1, ref data } => {
                            output_buffer.extend_from_slice(data);
                        }
                        ChannelMsg::ExitStatus { exit_status } => {
                            code = Some(exit_status);
                        }
                        _ => {}
                    }
                }

                // 使用 from_utf8_lossy 更安全，避免因非UTF8字符导致panic
                let output_string = String::from_utf8_lossy(&output_buffer).to_string();
                let exit_code = code.context("远程命令执行完毕，但未收到退出码")?;

                Ok(CallResult::Once((exit_code, output_string)))
            }
            ExecutionMode::Interactive => Ok(CallResult::Interactive(channel)),
        }
    }

    pub async fn call_once(&mut self, command: &str) -> Result<(u32, String)> {
        match self.call(command, ExecutionMode::Once).await? {
            CallResult::Once(result) => Ok(result),
            CallResult::Interactive(_) => Err(anyhow::anyhow!(
                "内部逻辑错误：请求 Once 模式但收到了 Interactive 结果"
            )),
        }
    }

    pub async fn upload_file(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let channel = self.session.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;

        // 创建父目录（如果不存在）
        if let Some(parent_dir) = std::path::Path::new(remote_path).parent() {
            if let Some(parent_str) = parent_dir.to_str() {
                // 尝试创建父目录，忽略错误（可能已存在）
                let _ = sftp.create_dir(parent_str).await;
            }
        }

        let mut file = sftp
            .open_with_flags(
                remote_path,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await?;

        file.write_all(data).await?;
        file.shutdown().await?;

        Ok(())
    }

    // 流式解压会因为缓冲区大小限制而失败，因此改为先上传到临时文件再解压
    pub async fn untar_from_memory(&mut self, data: &[u8], remote_dest_dir: &str) -> Result<()> {
        info!("远程文件大小: {}", format_size(data.len() as u64));
        // 1. 生成临时文件名
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let temp_file = format!("/tmp/upload_{}.tar.gz", timestamp);

        // 2. 使用 SFTP 上传数据到临时文件
        self.upload_file(&temp_file, data).await?;

        // 3. 创建目标目录并解压，然后删除临时文件
        let extract_cmd = format!(
            r#"mkdir -p "{}" && tar -xf "{}" -C "{}" && rm "{}""#,
            remote_dest_dir, temp_file, remote_dest_dir, temp_file
        );

        match self.call(&extract_cmd, ExecutionMode::Once).await? {
            CallResult::Once((code, output)) => {
                if code != 0 {
                    // 如果解压失败，尝试清理临时文件
                    let cleanup_cmd = format!("rm -f '{}'", temp_file);
                    let _ = self.call(&cleanup_cmd, ExecutionMode::Once).await;

                    return Err(anyhow!("解压失败，退出码: {}, 输出: {}", code, output));
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    pub async fn close(&mut self) -> Result<()> {
        self.session
            .disconnect(Disconnect::ByApplication, "", "English")
            .await?;
        Ok(())
    }
}
