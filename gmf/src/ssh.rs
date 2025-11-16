use std::{sync::Arc, time::Duration};

use crate::config::Config;
use anyhow::{Context, Result, anyhow, bail, ensure};
use russh::{keys::*, *};
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

/// 一次性执行命令的返回结果
#[derive(Debug)]
pub struct ExecOutput {
    pub exit_code: u32,
    pub output: String,
}

pub struct SSHSession {
    session: client::Handle<ClientHandle>,
}

impl SSHSession {
    pub async fn connect(cfg: &Config) -> Result<Self> {
        let config = client::Config {
            inactivity_timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        };
        let config = Arc::new(config);

        let sh = ClientHandle {};
        let addr = (cfg.host.as_str(), cfg.port);

        // 建立 TCP + SSH 握手
        let mut session = client::connect(config, addr, sh).await?;

        // 认证
        let auth_res = match (&cfg.password, &cfg.private_key_path) {
            (Some(password), _) => {
                session
                    .authenticate_password(cfg.user.as_str(), password)
                    .await?
            }
            (None, Some(private_key_path)) => {
                let key_pair = load_secret_key(private_key_path, None)?;
                let hash_alg = session.best_supported_rsa_hash().await?.flatten();
                let pk = PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash_alg);

                session
                    .authenticate_publickey(cfg.user.as_str(), pk)
                    .await?
            }
            (None, None) => {
                bail!("未配置认证方式：需要密码或私钥路径");
            }
        };

        ensure!(auth_res.success(), "SSH 认证失败");

        Ok(Self { session })
    }

    /// 内部工具：打开一个执行命令的 channel，并发送 exec
    async fn open_exec_channel(&mut self, command: &str) -> Result<Channel<client::Msg>> {
        let channel = self.session.channel_open_session().await?;
        channel.exec(true, command).await?;
        Ok(channel)
    }

    /// 一次性执行命令，等待退出码和全部输出
    pub async fn exec_once(&mut self, command: &str) -> Result<ExecOutput> {
        let mut channel = self.open_exec_channel(command).await?;

        let mut code = None;
        let mut output_buffer = Vec::new();

        while let Some(msg) = channel.wait().await {
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
        let output = String::from_utf8_lossy(&output_buffer).to_string();
        let exit_code = code.context("远程命令执行完毕，但未收到退出码")?;

        Ok(ExecOutput { exit_code, output })
    }

    /// 交互式执行命令：返回底层 Channel，由调用方自己收发数据
    pub async fn exec_interactive(&mut self, command: &str) -> Result<Channel<client::Msg>> {
        self.open_exec_channel(command).await
    }

    async fn upload_file(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let channel = self.session.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;

        // 创建父目录（如果不存在）
        if let Some(parent_dir) = std::path::Path::new(remote_path).parent()
            && let Some(parent_str) = parent_dir.to_str() {
                // 尝试创建父目录，忽略错误（可能已存在）
                let _ = sftp.create_dir(parent_str).await;
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

    // 流式解压会因为缓冲区大小限制而失败，因此改为先上传到/tmp目录下再解压
    pub async fn untar_from_memory(&mut self, data: &[u8], remote_dest_dir: &str) -> Result<()> {
        // 生成临时文件名
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let temp_file = format!("/tmp/gmf_upload_{timestamp}.tar.gz");

        // 使用 SFTP 上传数据到/tmp目录下
        self.upload_file(&temp_file, data).await?;

        // 创建目标目录并解压，然后删除临时文件
        let extract_cmd = format!(
            r#"mkdir -p "{remote_dest_dir}" && tar -xf "{temp_file}" -C "{remote_dest_dir}" && rm "{temp_file}""#
        );

        let ExecOutput { exit_code, output } = self.exec_once(&extract_cmd).await?;

        if exit_code != 0 {
            return Err(anyhow!("解压失败，退出码: {}, 输出: {}", exit_code, output));
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
