use anyhow::{Context, Result};
use bytes::Bytes;
use s3::creds::Credentials;
use s3::region::Region;
use s3::{Bucket, BucketConfiguration};
use tokio::sync::OnceCell;

use crate::config;

pub const BUCKET_NAME: &str = "gmf";

// 用于对象操作的共享存储桶实例
static BUCKET_INSTANCE: OnceCell<Box<Bucket>> = OnceCell::const_new();

// 内部函数，用于获取或初始化共享的 Bucket 实例，用于对象操作
async fn get_bucket() -> Result<&'static Box<Bucket>> {
    BUCKET_INSTANCE
        .get_or_try_init(|| async {
            let config = config::load_or_create_config().context("加载或创建配置文件失败")?;

            let region = Region::Custom {
                region: "auto".to_string(),
                endpoint: config.endpoint.clone(),
            };

            let credentials = Credentials::new(
                Some(&config.access_key_id),
                Some(&config.secret_access_key),
                None,
                None,
                None,
            )
            .context("无法创建 R2 凭证")?;
            let config = BucketConfiguration::default();

            let create_bucket_response =
                Bucket::create(BUCKET_NAME, region, credentials, config).await?;
            let bucket = create_bucket_response.bucket;

            let result: Result<Box<Bucket>> = Ok(bucket);
            result
        })
        .await
        .context("获取或初始化 Bucket 实例失败")
}

/// 从存储桶中删除一个对象
pub async fn delete_object(path: &str) -> Result<()> {
    let bucket = get_bucket().await?;
    bucket
        .delete_object(path)
        .await
        .context(format!("从存储桶删除对象 '{path}' 失败"))?;
    Ok(())
}

/// 从存储桶下载一个对象
pub async fn get_object(path: &str) -> Result<Bytes> {
    let bucket = get_bucket().await?;

    let response = bucket
        .get_object(path)
        .await
        .context(format!("从存储桶下载对象 '{path}' 失败"))?;

    // 检查 HTTP 状态码是否成功 (例如 200 OK)
    if response.status_code() != 200 {
        return Err(anyhow::anyhow!(
            "下载对象 '{}' 失败，状态码: {}, 响应体: {:?}",
            path,
            response.status_code(),
            String::from_utf8_lossy(response.bytes())
        ));
    }

    Ok(response.into_bytes())
}

pub async fn delete_bucket() -> Result<()> {
    let bucket = get_bucket().await?;

    bucket
        .delete()
        .await
        .context(format!("删除存储桶 '{BUCKET_NAME}' 失败"))?;

    Ok(())
}
