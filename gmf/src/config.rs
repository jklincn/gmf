use anyhow::{Context, Result};
use gmf_common::r2;
use gmf_common::utils::config_path;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    sync::OnceLock,
};

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

fn write_default_config() -> Result<()> {
    let path = config_path();

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

# 私钥路径 (Windows文件路径注意使用单引号或双反斜杠, 例如: 'C:\\Users\\user\\.ssh\\id_rsa')
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
        default.password.as_deref().unwrap_or(""),
        default.private_key_path.as_deref().unwrap_or(""),
        default.endpoint,
        default.access_key_id,
        default.secret_access_key
    );

    fs::write(&path, config_content).context("写入默认配置失败")?;
    Ok(())
}

fn load_or_create_config() -> Result<Option<Config>> {
    let path = config_path();
    if path.exists() {
        let content = fs::read_to_string(path).context("读取配置文件失败")?;
        let cfg: Config = toml::from_str(&content).context("解析配置文件失败")?;
        return Ok(Some(cfg));
    }
    write_default_config()?;
    Ok(None)
}

pub fn reset_config() -> Result<()> {
    let path = config_path();
    if path.exists() {
        println!("检测到已有配置文件：{}", path.display());
        print!("是否确认重置为默认配置？(y/N): ");
        io::stdout().flush().ok();
        let mut input = String::new();
        io::stdin().read_line(&mut input).ok();
        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            println!("操作已取消，未修改配置文件。");
            return Ok(());
        }
        fs::remove_file(&path).context("删除旧配置文件失败")?;
    }

    write_default_config()?;

    println!("配置文件已创建");
    println!("路径: {}", path.display());

    Ok(())
}

pub async fn init_r2() -> Result<()> {
    let cfg = get_config();
    let s3_config = r2::S3Config {
        endpoint: cfg.endpoint.clone(),
        access_key_id: cfg.access_key_id.clone(),
        secret_access_key: cfg.secret_access_key.clone(),
    };
    r2::init_s3(Some(s3_config))
        .await
        .context("连接 Cloudflare R2 失败")?;
    Ok(())
}

static GLOBAL_CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get_config() -> &'static Config {
    GLOBAL_CONFIG.get_or_init(|| {
        let result = load_or_create_config().expect("读取配置失败");

        match result {
            Some(cfg) => cfg,
            None => {
                println!("配置文件已创建，请根据实际情况修改后重新运行程序。");
                println!("路径: {}", config_path().display());
                std::process::exit(0);
            }
        }
    })
}
