use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use s3::Bucket;
use s3::creds::Credentials;
use s3::region::Region;
use tokio::sync::OnceCell;

use crate::config;

pub const BUCKET_NAME: &str = "gmf";

// 用于对象操作的共享存储桶实例
static BUCKET_INSTANCE: OnceCell<Box<Bucket>> = OnceCell::const_new();

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

            let bucket = Bucket::new(BUCKET_NAME, region, credentials)?;

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

async fn clear_bucket() -> Result<()> {
    let bucket = get_bucket().await?;

    // 获取存储桶中的所有对象
    let list_results = bucket.list("/".to_string(), None).await?;
    let all_keys: Vec<String> = list_results
        .into_iter()
        .flat_map(|page| page.contents)
        .map(|object| object.key)
        .collect();

    if all_keys.is_empty() {
        return Ok(());
    }

    let delete_futures = stream::iter(all_keys).map(|key| async move {
        match bucket.delete_object(&key).await {
            Ok(_) => Ok(()),
            Err(e) => {
                eprintln!("删除对象 '{}' 失败: {}", key, e);
                Err(anyhow::anyhow!("删除对象 '{}' 失败", key))
            }
        }
    });

    let results: Vec<Result<()>> = delete_futures.buffer_unordered(10).collect().await;

    for result in results {
        result?;
    }

    Ok(())
}

pub async fn delete_bucket() -> Result<()> {
    let bucket = get_bucket().await?;
    clear_bucket().await?;
    bucket
        .delete()
        .await
        .context(format!("删除存储桶 '{BUCKET_NAME}' 失败"))?;
    Ok(())
}
