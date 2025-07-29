use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use anyhow::Result;
use log::info;
use russh::keys::*;
use russh::*;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::AsyncWriteExt;

struct Client {}

impl client::Handler for Client {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}
// ---------------------------------

pub struct Session {
    session: client::Handle<Client>,
}

impl Session {
    pub async fn connect(cfg: &Config) -> Result<Self> {
        let key_pair = load_secret_key(&cfg.private_key_path, None)?;

        let config = client::Config {
            inactivity_timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };

        let config = Arc::new(config);
        let sh = Client {};

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
            anyhow::bail!("Authentication (with publickey) failed");
        }

        Ok(Self { session })
    }

    pub async fn call(&mut self, command: &str) -> Result<(u32, String)> {
        let mut channel = self.session.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut code = None;
        let mut output_buffer = Vec::new();

        loop {
            let Some(msg) = channel.wait().await else {
                break;
            };
            match msg {
                ChannelMsg::Data { ref data } => {
                    output_buffer.extend_from_slice(data);
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    code = Some(exit_status);
                }
                _ => {}
            }
        }
        let output_string = String::from_utf8(output_buffer)?;
        Ok((code.expect("程序没有正常退出，未收到退出码"), output_string))
    }

    pub async fn upload_file(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        info!("正在为上传操作启动 SFTP 子系统...");

        let channel = self.session.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;

        info!("SFTP 会话已建立，准备上传到 {}", remote_path);
        let mut file = sftp
            .open_with_flags(
                remote_path,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await?;
        file.write_all(data).await?;
        file.shutdown().await?;

        info!("文件成功上传到 {}，SFTP 会话结束。", remote_path);

        Ok(())
    }
    // ------------------------------------------

    pub async fn close(&mut self) -> Result<()> {
        self.session
            .disconnect(Disconnect::ByApplication, "", "English")
            .await?;
        Ok(())
    }
}
