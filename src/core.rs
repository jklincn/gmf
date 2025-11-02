use crate::config::get_config;

use anyhow::{Context, Result};
use reqwest::header::AUTHORIZATION;
use reqwest::multipart;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

static WORKER_JS: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/worker.js"));

fn etag_path() -> PathBuf {
    let base = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
        .expect("Cannot determine cache directory");

    let gmf_dir = base.join("gmf");
    fs::create_dir_all(&gmf_dir).expect("Failed to create cache directory");
    gmf_dir.join("worker_etag")
}

pub async fn create_worker() -> Result<()> {
    let account_id = std::env::var("CF_ACCOUNT_ID")?;
    let api_token = std::env::var("CF_API_TOKEN")?;
    let script_name = "gmf";

    let list_url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/workers/scripts",
        account_id
    );
    let put_url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/workers/scripts/{}",
        account_id, script_name
    );

    let client = reqwest::Client::new();

    // === Step 1: 获取所有 Worker 列表 ===
    let list_resp = client
        .get(&list_url)
        .header(AUTHORIZATION, format!("Bearer {}", api_token))
        .send()
        .await
        .context("请求 List Workers 失败")?;

    if !list_resp.status().is_success() {
        anyhow::bail!("❌ List workers 失败：状态 {}", list_resp.status());
    }

    let text = list_resp.text().await?;
    let json: serde_json::Value = serde_json::from_str(&text)?;

    let remote_etag = json["result"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|w| w["id"] == script_name)
                .and_then(|w| w["etag"].as_str())
        })
        .map(|s| s.to_string());

    let etag_file = etag_path();
    let local_etag = fs::read_to_string(&etag_file).ok();

    if let (Some(local), Some(remote)) = (&local_etag, &remote_etag) {
        if local.trim() == remote.trim() {
            println!(
                "✅ Worker '{script_name}' 已是最新版本（ETag={}），跳过上传。",
                remote
            );
            return Ok(());
        } else {
            println!("⚙️ Worker '{script_name}' 存在但版本不同，准备更新。");
        }
    } else if remote_etag.is_none() {
        println!("ℹ️ Worker '{script_name}' 不存在，将创建新版本。");
    }
    // === Step 2: 上传 worker ===
    let today = chrono::Utc::now().date_naive().to_string();
    let main_part_name = format!("{}.mjs", script_name);
    let metadata = json!({
        "main_module": main_part_name,
        "compatibility_date": today,
    });
    let metadata_text = serde_json::to_string_pretty(&metadata)?;

    let form = multipart::Form::new()
        .part(
            "metadata",
            multipart::Part::text(metadata_text).mime_str("application/json")?,
        )
        .part(
            main_part_name.clone(),
            multipart::Part::bytes(WORKER_JS.to_vec())
                .mime_str("application/javascript+module")?
                .file_name(main_part_name.clone()),
        );

    let put_resp = client
        .put(&put_url)
        .header(AUTHORIZATION, format!("Bearer {}", api_token))
        .multipart(form)
        .send()
        .await
        .context("上传请求失败")?;

    let status = put_resp.status();
    let text = put_resp.text().await?;

    if !status.is_success() {
        anyhow::bail!("❌ 上传失败：状态 {}，响应：{}", status, text);
    }

    println!("✅ 上传成功：{}", text);

    // === Step 3: 更新本地 etag 缓存 ===
    let result: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(etag) = result["result"]["etag"].as_str() {
        fs::write(&etag_file, etag)?;
        println!("📦 已更新本地缓存 ETag={}", etag);
    }
    Ok(())
}

pub async fn enable_workers_dev() -> Result<()> {
    let account_id = std::env::var("CF_ACCOUNT_ID")?;
    let api_token = std::env::var("CF_API_TOKEN")?;
    let script_name = "gmf";
    let client = reqwest::Client::new();
    let enable_subdomain_url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/workers/scripts/{}/subdomain",
        account_id, script_name
    );
    let body = json!({
        "enabled": true,
        "previews_enabled": false
    });
    let enable_subdomain_resp = client
        .post(&enable_subdomain_url)
        .header(AUTHORIZATION, format!("Bearer {}", api_token))
        .json(&body)
        .send()
        .await
        .context("启用 workers.dev 失败")?;
    let status = enable_subdomain_resp.status();
    let text = enable_subdomain_resp.text().await?;

    if !status.is_success() {
        anyhow::bail!("❌ 启用 workers.dev 失败：状态 {}，响应：{}", status, text);
    }
    println!("✅ workers.dev 域名启用成功：{}", text);
    Ok(())
}
