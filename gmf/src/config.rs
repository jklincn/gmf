use crate::ssh::{SSHConfig, SSHSession};
use anyhow::{Context, Result, anyhow, bail};
use gmf_common::utils::{config_path, resolve_path};
use gmf_common::{r2, utils::calc_xxh3};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Config {
    pub time: u64,
    pub ssh_config: SSHConfig,
    pub r2_config: r2::R2Config,
    pub hash: String,
}

impl Config {
    fn calculate_hash(&self) -> Result<String> {
        let mut temp = self.clone();
        // 计算哈希前必须将 xxh3 字段置空，防止循环依赖，并确保验证的一致性
        temp.hash = String::new();
        let content = serde_json::to_string(&temp).unwrap_or_default();
        calc_xxh3(content.as_str())
    }
}

fn save_config(config: &Config) -> Result<()> {
    let path = config_path();
    let mut temp = config.clone();
    temp.time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    temp.hash = temp.calculate_hash()?;

    let content = serde_json::to_string_pretty(&temp).context("序列化配置失败")?;
    fs::write(&path, content).context("写入配置文件失败")?;
    Ok(())
}

fn load_config() -> Result<Config> {
    let path = config_path();

    if !path.exists() {
        return Err(anyhow!("配置文件不存在"));
    }

    let content = fs::read_to_string(&path).context("读取配置文件失败")?;
    let config: Config = serde_json::from_str(&content).context("解析配置文件失败")?;

    if config.hash != config.calculate_hash()? {
        bail!("配置文件 {} 数据校验失败，请重新登录设置", path.display());
    }

    Ok(config)
}

fn prompt(label: &str) -> String {
    print!("{}: ", label);
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    input.trim().to_string()
}

pub async fn login() -> Result<()> {
    let path = config_path();
    if path.exists() {
        let input = prompt("配置文件已存在，是否覆盖？(y/N)");
        if !input.eq_ignore_ascii_case("y") {
            println!("已取消配置更改。");
            return Ok(());
        }
    }

    println!("\n=== SSH 配置 ===");

    let mut ssh_config = SSHConfig::default();

    ssh_config.hostname = prompt("主机地址");
    let port_str = prompt("端口");
    if let Ok(p) = port_str.parse() {
        ssh_config.port = p;
    } else {
        bail!("无效的端口号");
    }
    ssh_config.user = prompt("用户名");
    let key = prompt("私钥路径 (推荐，回车跳过)");
    ssh_config.private_key_path = if key.is_empty() {
        None
    } else {
        Some(resolve_path(&key).to_string_lossy().to_string())
    };

    let pass_label = if ssh_config.private_key_path.is_some() {
        "密码 (已设置私钥，回车跳过)"
    } else {
        "密码"
    };
    let pass = prompt(pass_label);
    ssh_config.password = if pass.is_empty() { None } else { Some(pass) };

    // 测试 ssh 连接
    let mut ssh_session = SSHSession::connect(&ssh_config)
        .await
        .context("SSH 连接失败")?;
    ssh_session.close().await?;
    println!("SSH 连接成功!");

    println!("\n=== Cloudflare R2 配置 ===");
    let mut r2_config = r2::R2Config::default();
    r2_config.endpoint = prompt("S3 API Endpoint");
    r2_config.access_key_id = prompt("AccessKeyID");
    r2_config.secret_access_key = prompt("SecretAccessKey");

    // 测试 r2 连接
    r2::init(Some(r2_config.clone()))
        .await
        .with_context(|| "连接 Cloudflare R2 失败")?;
    r2::delete_bucket()
        .await
        .with_context(|| "删除 R2 Bucket 失败")?;
    println!("Cloudflare R2 连接成功!");

    let config = Config {
        ssh_config,
        r2_config,
        ..Default::default()
    };
    save_config(&config).context("保存配置失败")?;
    println!("\n=== 配置完成 ===");
    println!("配置已保存至 {}", config_path().display());
    Ok(())
}

pub async fn init_r2() -> Result<()> {
    let cfg = get_config()?;
    let s3_config = r2::R2Config {
        endpoint: cfg.r2_config.endpoint.clone(),
        access_key_id: cfg.r2_config.access_key_id.clone(),
        secret_access_key: cfg.r2_config.secret_access_key.clone(),
    };
    r2::init(Some(s3_config))
        .await
        .context("连接 Cloudflare R2 失败")?;
    Ok(())
}

static GLOBAL_CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get_config() -> Result<&'static Config> {
    if let Some(cfg) = GLOBAL_CONFIG.get() {
        return Ok(cfg);
    }

    let cfg = load_config()?;
    let _ = GLOBAL_CONFIG.set(cfg);

    Ok(GLOBAL_CONFIG.get().expect("配置应当已被初始化"))
}
