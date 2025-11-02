use anyhow::{anyhow, Context, Result};
use dialoguer::Input;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct AppConfig {
    pub account_id: String,
    pub api_token: String,
    pub subdomain: String,
}

static CONFIG: OnceCell<AppConfig> = OnceCell::new();

fn config_path() -> PathBuf {
    let base = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .expect("Cannot determine config directory");

    let gmf_dir = base.join("gmf");
    fs::create_dir_all(&gmf_dir).expect("Failed to create config directory");
    gmf_dir.join("config.toml")
}

pub fn prompt_config() -> Result<()> {
    let account_id: String = Input::new().with_prompt("account_id").interact_text()?;
    let api_token: String = Input::new().with_prompt("api_token").interact_text()?;
    let subdomain: String = Input::new().with_prompt("subdomain").interact_text()?;
    let cfg = AppConfig {
        account_id,
        api_token,
        subdomain,
    };
    let path = config_path();
    let content = toml::to_string_pretty(&cfg)?;
    fs::write(&path, content)?;
    println!("✅ Config saved to {}", path.display());
    Ok(())
}

pub fn load_config() -> Result<()> {
    let path = config_path();
    if !path.exists() {
        return Err(anyhow!(
            "No config found. Please run `gmf config` first.\nExpected at: {}",
            path.display()
        ));
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let cfg: AppConfig = toml::from_str(&raw)
        .with_context(|| format!("Failed to parse TOML at {}", path.display()))?;

    CONFIG
        .set(cfg)
        .map_err(|_| anyhow!("CONFIG already initialized"))?;

    Ok(())
}

pub fn get_config() -> &'static AppConfig {
    CONFIG.get().expect("Config not loaded, call load_config() first")
}
