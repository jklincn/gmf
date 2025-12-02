use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use s3::creds::Credentials;
use s3::region::Region;
use s3::{Bucket, BucketConfiguration};
use s3::{retry, set_retries};
use serde::{Deserialize, Serialize};
use std::env;
use tokio::sync::OnceCell;

use crate::consts::S3_RETRY;
static BUCKET: OnceCell<Box<Bucket>> = OnceCell::const_new();

const BUCKET_NAME: &str = "gmf";

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct R2Config {
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub async fn init(config_override: Option<R2Config>) -> Result<()> {
    set_retries(S3_RETRY);
    let use_override = config_override.is_some();
    let cfg = match config_override {
        Some(cfg) => cfg,
        None => R2Config {
            endpoint: env::var("ENDPOINT").context("环境变量 'ENDPOINT' 未设置")?,
            access_key_id: env::var("ACCESS_KEY_ID").context("环境变量 'ACCESS_KEY_ID' 未设置")?,
            secret_access_key: env::var("SECRET_ACCESS_KEY")
                .context("环境变量 'SECRET_ACCESS_KEY' 未设置")?,
        },
    };
    let region = Region::Custom {
        region: "us-east-1".to_string(),
        endpoint: cfg.endpoint.clone(),
    };
    let credentials = Credentials::new(
        Some(&cfg.access_key_id),
        Some(&cfg.secret_access_key),
        None,
        None,
        None,
    )?;

    if !use_override {
        let bucket =
            Bucket::new(BUCKET_NAME, region.clone(), credentials.clone())?.with_path_style();

        const MAX_RETRIES: usize = 5;
        const RETRY_DELAY_MS: u64 = 500;

        let mut ok = false;
        for _ in 1..=MAX_RETRIES {
            if bucket.exists().await? {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS)).await;
        }

        if !ok {
            return Err(anyhow!(
                "R2 bucket '{}' 在默认配置下未检测到存在（尝试 {} 次）",
                BUCKET_NAME,
                MAX_RETRIES
            ));
        }

        BUCKET
            .set(bucket)
            .map_err(|_| anyhow!("R2 Bucket 已初始化，不可重复初始化"))?;

        Ok(())
    } else {
        let bucket =
            Bucket::new(BUCKET_NAME, region.clone(), credentials.clone())?.with_path_style();
        let final_bucket = if !bucket.exists().await? {
            let config = BucketConfiguration::default();
            let resp = Bucket::create(BUCKET_NAME, region, credentials, config).await?;
            if resp.success() {
                resp.bucket
            } else {
                return Err(anyhow!(
                    "创建 R2 bucket 失败: HTTP {}, 响应内容：{}",
                    resp.response_code,
                    resp.response_text
                ));
            }
        } else {
            bucket
        };

        BUCKET
            .set(final_bucket)
            .map_err(|_| anyhow!("R2 Bucket 已初始化，不可重复初始化"))?;

        Ok(())
    }
}

pub fn get_bucket() -> Result<&'static Bucket> {
    BUCKET
        .get()
        .map(|b| b.as_ref())
        .context("R2 Bucket 尚未初始化，请先调用 init()")
}

pub async fn list_objects() -> Result<Vec<String>> {
    let bucket = get_bucket()?;
    let mut keys = Vec::new();
    let results = bucket
        .list("".to_string(), None)
        .await
        .context("列出对象失败")?;
    for page in results {
        for obj in page.contents {
            keys.push(obj.key);
        }
    }
    Ok(keys)
}

pub async fn delete_object(key: &str) -> Result<()> {
    let bucket = get_bucket()?;
    let resp = bucket.delete_object(key).await?;
    let code = resp.status_code();
    if code != 204 {
        return Err(anyhow!("删除对象失败, key={}, 状态码={}", key, code));
    }
    Ok(())
}

pub async fn delete_bucket() -> Result<()> {
    let bucket = get_bucket()?;
    let objects = list_objects().await?;
    if !objects.is_empty() {
        for key in objects {
            let resp = bucket.delete_object(&key).await?;
            let code = resp.status_code();
            if code != 204 {
                return Err(anyhow!("删除对象失败, key={}, 状态码={}", key, code));
            }
        }
    }
    bucket.delete().await?;
    Ok(())
}

pub async fn get_object(key: &str) -> Result<Bytes> {
    let bucket = get_bucket()?;
    let resp = retry!(bucket.get_object(key).await)?;
    let code = resp.status_code();
    if code != 200 {
        return Err(anyhow!(
            "Failed to download object, key={}, status_code={}",
            key,
            code
        ));
    }
    Ok(Bytes::copy_from_slice(resp.as_slice()))
}

pub async fn put_object(key: &str, data: &[u8]) -> Result<()> {
    let bucket = get_bucket()?;
    let resp = retry!(bucket.put_object(key, data).await)?;
    let code = resp.status_code();
    if code != 200 {
        return Err(anyhow!(
            "Failed to upload object, key={}, status_code={}",
            key,
            code
        ));
    }
    Ok(())
}
