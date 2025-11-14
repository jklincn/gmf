use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use std::env;
use tokio::sync::OnceCell;

use s3::creds::Credentials;
use s3::region::Region;
use s3::{Bucket, BucketConfiguration};

static S3_CONFIG: OnceCell<S3Config> = OnceCell::const_new();
static BUCKET: OnceCell<Box<Bucket>> = OnceCell::const_new();

const BUCKET_NAME: &str = "gmf";

#[derive(Clone, Debug)]
pub struct S3Config {
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub async fn init_s3(config_override: Option<S3Config>) -> Result<()> {
    S3_CONFIG
        .set(match config_override {
            Some(cfg) => cfg,
            None => S3Config {
                endpoint: env::var("ENDPOINT").context("环境变量 'ENDPOINT' 未设置")?,
                access_key_id: env::var("ACCESS_KEY_ID")
                    .context("环境变量 'ACCESS_KEY_ID' 未设置")?,
                secret_access_key: env::var("SECRET_ACCESS_KEY")
                    .context("环境变量 'SECRET_ACCESS_KEY' 未设置")?,
            },
        })
        .map_err(|_| anyhow!("S3 配置已初始化，不可重复初始化"))?;

    let cfg = S3_CONFIG.get().unwrap();
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
    let bucket = Bucket::new(BUCKET_NAME, region.clone(), credentials.clone())?.with_path_style();
    let final_bucket = if !bucket.exists().await? {
        let config = BucketConfiguration::default();
        let resp = Bucket::create(BUCKET_NAME, region, credentials, config).await?;
        if resp.success() {
            resp.bucket
        } else {
            return Err(anyhow!("创建 S3 bucket 失败"));
        }
    } else {
        bucket
    };

    BUCKET
        .set(final_bucket)
        .map_err(|_| anyhow!("S3 Bucket 已初始化，不可重复初始化"))?;

    Ok(())
}

pub fn get_bucket() -> Result<&'static Bucket> {
    BUCKET
        .get()
        .map(|b| b.as_ref())
        .context("S3 Bucket 尚未初始化，请先调用 init_s3()")
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
    let resp = bucket.get_object(key).await?;
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
    let resp = bucket.put_object(key, data).await?;
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
