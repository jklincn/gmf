use anyhow::{Context, Result};
use gmf_common::r2;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub private_key_path: Option<String>,

    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub fn config_path() -> PathBuf {
    let base = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .expect("Cannot determine config directory");

    let gmf_dir = base.join("gmf");
    fs::create_dir_all(&gmf_dir).expect("Failed to create config directory");
    gmf_dir.join("config.toml")
}

/// 加载或创建配置文件
pub fn load_or_create_config() -> Result<Option<Config>> {
    let path = config_path();
    if path.exists() {
        let content = fs::read_to_string(path).context("读取配置文件失败")?;
        let cfg: Config = toml::from_str(&content).context("解析配置文件失败")?;
        return Ok(Some(cfg))
    }
    // 创建一个默认配置的实例
    let default = Config {
        host: "192.168.1.1".into(),
        port: 22,
        user: "user".into(),
        password: Some("password".into()),
        private_key_path: Some("your_private_key_path".into()),
        endpoint: "https://xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx.r2.cloudflarestorage.com".into(),
        access_key_id: "your_access_key_id".into(),
        secret_access_key: "your_secret_access_key".into(),
    };

    let config_content = format!(
        r#"# =============== SSH 连接配置 ================

# 目标主机IP或域名
host = "{}"

# SSH端口
port = {}

# 用户名
user = "{}"

# 密码 (推荐使用密钥登陆，如果启用密码，则会忽略密钥)
# password = "{}"

# 私钥路径 (Windows中文件路径注意使用单引号或双反斜杠, 例如: 'C:\\Users\\user\\.ssh\\id_rsa')
private_key_path = '{}'

# ======== Cloudflare R2 对象存储配置 =========

# R2 API 的 Endpoint 地址
endpoint = "{}"

# Cloudflare R2 访问密钥ID
access_key_id = "{}"

# Cloudflare R2 机密访问密钥
secret_access_key = "{}"
"#,
        default.host,
        default.port,
        default.user,
        default.password.as_deref().unwrap(),
        default.private_key_path.as_deref().unwrap(),
        default.endpoint,
        default.access_key_id,
        default.secret_access_key
    );

    fs::write(&path, config_content).context("写入默认配置失败")?;
    Ok(None)
}

pub async fn set_r2(cfg: &Config) -> Result<()> {
    let s3_config = r2::S3Config {
        endpoint: cfg.endpoint.clone(),
        access_key_id: cfg.access_key_id.clone(),
        secret_access_key: cfg.secret_access_key.clone(),
    };
    r2::init_s3(Some(s3_config)).await?;
    Ok(())
}
