use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

const CONFIG_FILE_NAME: &str = "config.toml";

/// 配置结构体
#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub private_key_path: Option<String>,
    pub endpoint: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

/// 加载或创建配置文件
pub fn load_or_create_config() -> Result<Config> {
    let path = Path::new(CONFIG_FILE_NAME);
    if path.exists() {
        let content = fs::read_to_string(path).context("读取配置文件失败")?;
        let cfg: Config = toml::from_str(&content).context("解析配置文件失败")?;
        Ok(cfg)
    } else {
        let default = Config {
            host: "192.168.1.100".into(),
            port: 22,
            user: "root".into(),
            password: Some("your_password".into()),
            private_key_path: Some("your_private_key_path".into()),
            endpoint: Some("https://example.com".into()),
            access_key_id: Some("your_access_key_id".into()),
            secret_access_key: Some("your_secret_access_key".into()),
        };
        let header = "# SSH 连接配置\n\
              # host: 目标主机IP或域名\n\
              # port: SSH端口\n\
              # user: 登录用户名\n\
              # password: 登录密码\n\
              # private_key_path: 私钥路径（Windows路径使用单引号包裹）\n\
              # 如果同时设置了 password 和 private_key_path，则优先使用私钥登录\n\n\
              # Cloudfalre R2 对象存储配置\n\
              # access_key_id: Cloudflare R2 访问密钥ID\n\
              # secret_access_key: Cloudflare R2 机密访问密钥\n\n";

        let mut body = toml::to_string_pretty(&default).context("序列化默认配置失败")?;
        body = body.replace(
            "private_key_path = \"your_private_key_path\"",
            "private_key_path = 'your_private_key_path'",
        );
        fs::write(path, format!("{}{}", header, body)).context("写入默认配置失败")?;
        println!(
            "已生成默认配置文件 '{}', 请根据实际情况修改配置信息",
            CONFIG_FILE_NAME
        );
        std::process::exit(0);
    }
}
