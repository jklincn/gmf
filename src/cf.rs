use crate::config::get_config;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use httpdate::parse_http_date;
use reqwest::Response;
use reqwest::header::{AUTHORIZATION, RETRY_AFTER};
use reqwest::multipart;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

static WORKER_JS: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/worker.js"));

fn version_path() -> PathBuf {
    let base = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
        .expect("Cannot determine cache directory");

    let gmf_dir = base.join("gmf");
    fs::create_dir_all(&gmf_dir).expect("Failed to create cache directory");
    gmf_dir.join("install_version")
}

/// Cloudflare API 统一封装
struct CloudflareClient {
    http: reqwest::Client,
    base: String,
    script_name: String,

    account_id: String,
    token: String,
    subdomain: String,
}

impl CloudflareClient {
    fn new() -> Self {
        let cfg = get_config();
        let http = reqwest::Client::builder()
            .user_agent("gmf-cli/0.1 (+https://example.local)")
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            base: "https://api.cloudflare.com/client/v4".to_string(),
            script_name: "gmf".to_string(),
            account_id: cfg.account_id.clone(),
            token: cfg.api_token.clone(),
            subdomain: cfg.subdomain.clone(),
        }
    }

    #[inline]
    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    #[inline]
    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    /// 简单的带重试请求执行器（429/5xx 重试），用于 JSON/文本 API
    async fn send_with_retry<F>(&self, mut build: F) -> Result<Response>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut attempt = 0usize;
        let max_retries = 3usize;
        let mut backoff = Duration::from_millis(300);

        loop {
            let res = build().send().await;

            match res {
                Ok(resp) => {
                    if resp.status().is_success() {
                        return Ok(resp);
                    }

                    let status = resp.status();
                    if status.as_u16() == 429 || status.is_server_error() {
                        attempt += 1;

                        // honor Retry-After
                        let mut delay = backoff;
                        if let Some(v) = resp.headers().get(RETRY_AFTER) {
                            if let Ok(s) = v.to_str() {
                                if let Ok(sec) = s.parse::<u64>() {
                                    delay = Duration::from_secs(sec);
                                } else if let Ok(dt) = parse_http_date(s) {
                                    let now = std::time::SystemTime::now();
                                    delay =
                                        dt.duration_since(now).unwrap_or(Duration::from_secs(0));
                                }
                            }
                        }

                        if attempt <= max_retries {
                            sleep(delay).await;
                            backoff = backoff.saturating_mul(2);
                            continue;
                        }
                    }

                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("HTTP {}: {}", status, text));
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > max_retries {
                        return Err(anyhow!("request error after retries: {}", e));
                    }
                    sleep(backoff).await;
                    backoff = backoff.saturating_mul(2);
                }
            }
        }
    }

    async fn upload_worker(&self) -> Result<()> {
        let url = self.url(&format!(
            "/accounts/{}/workers/scripts/{}",
            self.account_id, self.script_name
        ));
        let today = Utc::now().date_naive().to_string();
        let main_part_name = format!("{}.mjs", self.script_name);

        // 这些数据要在闭包里复用，提前 clone 到 owned
        let auth = self.auth_header();
        let main_part_name_owned = main_part_name.clone();
        let metadata_text = serde_json::to_string(&json!({
            "main_module": main_part_name,
            "compatibility_date": today,
        }))?;

        // 重试时每次重建 multipart 与请求
        self.send_with_retry(|| {
            let form = multipart::Form::new()
                .part(
                    "metadata",
                    multipart::Part::text(metadata_text.clone())
                        .mime_str("application/json")
                        .expect("set metadata mime"),
                )
                .part(
                    main_part_name_owned.clone(),
                    multipart::Part::bytes(WORKER_JS.to_vec())
                        .mime_str("application/javascript+module")
                        .expect("set main module mime")
                        .file_name(main_part_name_owned.clone()),
                );

            self.http
                .put(&url)
                .header(AUTHORIZATION, auth.clone())
                .header("Accept", "application/json")
                .multipart(form)
        })
        .await
        .context("upload worker failed")?;
        Ok(())
    }

    async fn install_worker(&self) -> Result<()> {
        let version_file = version_path();
        let local_version = fs::read_to_string(&version_file)
            .ok()
            .map(|s| s.trim().to_owned());
        let current_version = env!("CARGO_PKG_VERSION");
        if local_version.is_none() || current_version != local_version.unwrap() {
            self.upload_worker().await.context("upload worker failed")?;
            fs::write(&version_file, &current_version.as_bytes())?;
        }
        Ok(())
    }

    async fn test_connection(&self) -> Result<()> {
        let url = self.url("/user/tokens/verify");
        let auth = self.auth_header();
        self.send_with_retry(|| {
            self.http
                .get(&url)
                .header(AUTHORIZATION, auth.clone())
                .header("Accept", "application/json")
        })
        .await
        .context("test connection failed")?;

        println!("Cloudflare API connection successful.");
        Ok(())
    }
}

pub async fn push() -> Result<()> {
    let cf = CloudflareClient::new();
    cf.test_connection().await?;
    cf.install_worker().await?;
    Ok(())
}
