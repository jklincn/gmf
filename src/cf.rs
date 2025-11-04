use crate::config::get_config;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use httpdate::parse_http_date;
use reqwest::header::{AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use reqwest::multipart;
use reqwest::Response;
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

// 你的 worker 打包产物
static WORKER_JS: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/worker.js"));

fn etag_path() -> PathBuf {
    let base = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
        .expect("Cannot determine cache directory");

    let gmf_dir = base.join("gmf");
    fs::create_dir_all(&gmf_dir).expect("Failed to create cache directory");
    gmf_dir.join("worker_etag")
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
                                    delay = dt.duration_since(now).unwrap_or(Duration::from_secs(0));
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

    /// 获取某个 Worker 的 ETag（若不存在返回 Ok(None)）
    ///
    /// 根据 Cloudflare 文档：列举脚本 `GET /accounts/{account_id}/workers/scripts`
    /// 返回 result: [ { id, etag, ... }, ... ]，这里按 id==script_name 取对应 etag。
    async fn get_worker_etag(&self) -> anyhow::Result<Option<String>> {
        let url = self.url(&format!("/accounts/{}/workers/scripts", self.account_id));

        #[derive(Deserialize, Debug)]
        struct WorkerRow {
            id: Option<String>,
            etag: Option<String>,
        }
        #[derive(Deserialize, Debug)]
        struct ListResp {
            success: bool,
            result: Option<Vec<WorkerRow>>,
            errors: Option<Vec<serde_json::Value>>,
        }

        // 用重试器请求列表（列表接口也可能偶发 5xx/429）
        let auth = self.auth_header();
        let resp = self
            .send_with_retry(|| {
                self.http
                    .get(&url)
                    .header(AUTHORIZATION, auth.clone())
                    .header(USER_AGENT, "gmf-cli/0.1")
                    .header("Accept", "application/json")
                    // 可按需放大分页（SinglePage 也会返回全量，但保险起见）
                    .query(&[("per_page", "1000")])
            })
            .await
            .context("list workers failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "get_worker_etag failed: HTTP {} {}",
                status,
                text
            ));
        }

        let body: ListResp =
            serde_json::from_str(&text).context("error decoding response body")?;

        if !body.success {
            return Err(anyhow!("cloudflare API returned success=false: {:?}", body.errors));
        }

        let rows = match body.result {
            Some(v) => v,
            None => return Ok(None),
        };

        let target = rows
            .into_iter()
            .find(|r| r.id.as_deref() == Some(&self.script_name));

        Ok(target.and_then(|r| r.etag))
    }

    /// 上传（创建/更新）Worker；返回新的 ETag
    async fn upload_worker(&self) -> Result<String> {
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
        let resp = self
            .send_with_retry(|| {
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

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("upload failed: HTTP {} {}", status, text));
        }

        #[derive(Deserialize)]
        struct UploadResp {
            success: bool,
            result: UploadResult,
        }
        #[derive(Deserialize)]
        struct UploadResult {
            etag: String,
        }

        let body: UploadResp =
            resp.json().await.context("invalid JSON response for upload")?;

        if !body.success {
            return Err(anyhow!("cloudflare API returned success=false during upload"));
        }

        Ok(body.result.etag)
    }

    /// 启用 workers.dev 子域
    async fn enable_workers_dev(&self) -> Result<()> {
        let url = self.url(&format!(
            "/accounts/{}/workers/scripts/{}/subdomain",
            self.account_id, self.script_name
        ));
        let body = json!({
            "enabled": true,
            "previews_enabled": false
        });

        let auth = self.auth_header();
        let resp = self
            .send_with_retry(|| {
                self.http
                    .post(&url)
                    .header(AUTHORIZATION, auth.clone())
                    .header("Accept", "application/json")
                    .json(&body)
            })
            .await
            .context("enable workers.dev failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "enable workers.dev failed: HTTP {} {}",
                status,
                text
            ));
        }
        Ok(())
    }

    async fn create_worker(&self) -> Result<()> {
        // 远端 ETag（通过列表检索）
        let remote_etag = self
            .get_worker_etag()
            .await
            .context("query remote worker etag failed")?;

        // 本地缓存 ETag
        let etag_file = etag_path();
        let local_etag = fs::read_to_string(&etag_file)
            .ok()
            .map(|s| s.trim().to_owned());

        // 无需上传的快速路径
        if let (Some(local), Some(remote)) = (&local_etag, &remote_etag) {
            if local == remote {
                println!(
                    "✅ Worker '{}' is up-to-date (ETag={}). Skip upload.",
                    self.script_name, remote
                );
                return Ok(());
            } else {
                println!(
                    "⚙️  Worker '{}' exists but ETag differs. local={}, remote={}. Updating…",
                    self.script_name, local, remote
                );
            }
        } else if remote_etag.is_none() {
            println!("ℹ️ Worker '{}' does not exist. Creating…", self.script_name);
        }

        // 上传
        let new_etag = self
            .upload_worker()
            .await
            .context("upload worker failed")?;

        // 写回本地 ETag
        fs::write(&etag_file, &new_etag)?;
        println!("📦 Updated local ETag cache: {}", new_etag);
        Ok(())
    }

    async fn test_connection(&self) -> Result<()> {
        let url = self.url("/user/tokens/verify");
        let auth = self.auth_header();
        let resp = self
            .send_with_retry(|| {
                self.http
                    .get(&url)
                    .header(AUTHORIZATION, auth.clone())
                    .header("Accept", "application/json")
            })
            .await
            .context("test connection failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("test connection failed: HTTP {} {}", status, text));
        }

        println!("✅ Cloudflare API connection successful.");
        Ok(())
    }
}

pub async fn push() -> Result<()> {
    let cf = CloudflareClient::new();
    cf.test_connection().await?;
    cf.create_worker().await?;
    cf.enable_workers_dev().await?;
    Ok(())
}
