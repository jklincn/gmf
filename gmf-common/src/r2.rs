use anyhow::{Context, Result};
use aws_config::{self, BehaviorVersion, Region, retry::RetryConfig, timeout::TimeoutConfig};
use aws_sdk_s3::{self as s3, config::Credentials, error::ProvideErrorMetadata};
use bytes::Bytes;
use log::error;
use std::{env, time::Duration};
use tokio::{
    sync::OnceCell,
    time::{sleep, timeout},
};

static S3_CLIENT: OnceCell<s3::Client> = OnceCell::const_new();

const BUCKET_NAME: &str = "gmf";

#[derive(Clone, Debug)]
pub struct S3Config {
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub async fn init_s3_client(config_override: Option<S3Config>) -> Result<()> {
    if S3_CLIENT.get().is_some() {
        return Err(anyhow::anyhow!("S3 客户端已经被初始化"));
    }

    let (endpoint, access_key_id, secret_access_key, retry_config, timeout_config) =
        if let Some(config) = config_override.clone() {
            // 客户端配置
            // 超时和重试手动处理
            let retry_config = RetryConfig::standard()
                .with_max_attempts(5)
                .with_initial_backoff(Duration::from_millis(500))
                .with_max_backoff(Duration::from_millis(500));
            let timeout_config = TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(5))
                .operation_attempt_timeout(Duration::from_secs(10))
                .operation_timeout(Duration::from_secs(60))
                .build();
            (
                config.endpoint,
                config.access_key_id,
                config.secret_access_key,
                retry_config,
                timeout_config,
            )
        } else {
            // 服务端配置
            let endpoint = env::var("ENDPOINT").context("环境变量 'ENDPOINT' 未设置")?;
            let access_key_id =
                env::var("ACCESS_KEY_ID").context("环境变量 'ACCESS_KEY_ID' 未设置")?;
            let secret_access_key =
                env::var("SECRET_ACCESS_KEY").context("环境变量 'SECRET_ACCESS_KEY' 未设置")?;
            let retry_config = RetryConfig::standard()
                .with_max_attempts(3)
                .with_initial_backoff(Duration::from_millis(500))
                .with_max_backoff(Duration::from_millis(500));
            let timeout_config = TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(3))
                .operation_timeout(Duration::from_secs(60))
                .operation_attempt_timeout(Duration::from_secs(20))
                .build();
            (
                endpoint,
                access_key_id,
                secret_access_key,
                retry_config,
                timeout_config,
            )
        };

    // 构建配置
    let config = aws_config::defaults(BehaviorVersion::v2025_01_17())
        .endpoint_url(endpoint)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            access_key_id,
            secret_access_key,
            None,
            None,
            "R2",
        ))
        .retry_config(retry_config)
        .timeout_config(timeout_config)
        .load()
        .await;

    let client = s3::Client::new(&config);

    S3_CLIENT
        .set(client)
        .map_err(|_| anyhow::anyhow!("内部错误: 设置 S3 客户端失败，可能已被其他线程初始化"))?;

    // 服务端预热
    if config_override.is_none() {
        let client_to_warm_up = S3_CLIENT.get().unwrap();
        let _ = client_to_warm_up
            .put_object()
            .bucket(BUCKET_NAME)
            .key("0")
            .body(s3::primitives::ByteStream::from(Vec::new()))
            .send()
            .await;
        let _ = client_to_warm_up
            .delete_object()
            .bucket(BUCKET_NAME)
            .key("0")
            .send()
            .await;
    }

    Ok(())
}

fn get_s3_client() -> Result<&'static s3::Client> {
    S3_CLIENT.get().context("内部错误: S3 客户端尚未初始化")
}

pub async fn list_buckets() -> Result<Vec<String>> {
    let client = get_s3_client()?;
    let resp = client.list_buckets().send().await?;
    let buckets = resp.buckets();
    let bucket_names: Vec<String> = buckets
        .iter()
        .filter_map(|b| b.name().map(|n| n.to_string()))
        .collect();
    Ok(bucket_names)
}

pub async fn create_bucket() -> Result<()> {
    let client = get_s3_client()?;
    let exist_buckets = list_buckets().await?;
    if exist_buckets.contains(&BUCKET_NAME.to_string()) {
        return Ok(());
    }
    client.create_bucket().bucket(BUCKET_NAME).send().await?;
    Ok(())
}

pub async fn list_objects() -> Result<Vec<String>> {
    let client = get_s3_client()?;

    let list_objects_output = client.list_objects_v2().bucket(BUCKET_NAME).send().await?;
    let mut object_keys = Vec::new();
    for object in list_objects_output.contents() {
        if let Some(key) = object.key() {
            object_keys.push(key.to_string());
        }
    }
    Ok(object_keys)
}

pub async fn delete_object(key: &str) -> Result<()> {
    let client = get_s3_client()?;

    client
        .delete_object()
        .bucket(BUCKET_NAME)
        .key(key)
        .send()
        .await?;
    Ok(())
}

pub async fn delete_objects(objects_to_delete: Vec<String>) -> Result<()> {
    let client = get_s3_client()?;

    let mut delete_object_ids: Vec<aws_sdk_s3::types::ObjectIdentifier> = vec![];
    for obj in objects_to_delete {
        let obj_id = aws_sdk_s3::types::ObjectIdentifier::builder()
            .key(obj)
            .build()?;
        delete_object_ids.push(obj_id);
    }

    client
        .delete_objects()
        .bucket(BUCKET_NAME)
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .set_objects(Some(delete_object_ids))
                .build()?,
        )
        .send()
        .await?;
    Ok(())
}

pub async fn delete_bucket_with_retry() -> Result<()> {
    let client = get_s3_client()?;

    loop {
        // 尝试删除存储桶
        match client.delete_bucket().bucket(BUCKET_NAME).send().await {
            // 如果成功
            Ok(_) => {
                break; // 成功，跳出循环
            }
            // 如果失败
            Err(sdk_error) => {
                if let Some(service_error) = sdk_error.as_service_error() {
                    // 检查错误码是否为 "BucketNotEmpty"
                    if service_error.code() == Some("BucketNotEmpty") {
                        let objects = list_objects().await?;
                        if !objects.is_empty() {
                            delete_objects(objects).await?;
                        }

                        // 等待1秒后重试
                        sleep(Duration::from_secs(1)).await;
                        continue; // 继续下一次循环
                    }
                }

                // 如果错误不是 BucketNotEmpty，或者无法解析为服务错误，
                // 则认为是一个无法处理的致命错误，打印并返回。
                error!("内部错误: 删除存储桶时发生无法处理的错误: {sdk_error:?}");
                return Err(sdk_error.into());
            }
        }
    }

    Ok(())
}

// TODO：上传与下载的完整性验证
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
pub async fn get_object(key: &str) -> Result<Bytes> {
    let client = get_s3_client()?;
    let get_object_override_config = aws_sdk_s3::config::Builder::default()
        .retry_config(RetryConfig::disabled())
        .timeout_config(TimeoutConfig::disabled());

    // 手动控制下载 20 秒超时
    match timeout(ATTEMPT_TIMEOUT, async {
        let resp = client
            .get_object()
            .bucket(BUCKET_NAME)
            .key(key)
            .customize()
            .config_override(get_object_override_config.clone())
            .send()
            .await?;

        let body = resp
            .body
            .collect()
            .await
            .context("Failed to collect S3 object body during download")?;

        Ok(body.into_bytes())
    })
    .await
    {
        // 超时
        Err(_) => Err(anyhow::anyhow!(
            "Operation timed out after {:?}",
            ATTEMPT_TIMEOUT
        )),
        // 未超时，但内部有错误
        Ok(Err(e)) => Err(e),
        // 成功
        Ok(Ok(bytes)) => Ok(bytes),
    }
}

pub async fn put_object(key: &str, data: Vec<u8>) -> Result<()> {
    let client = get_s3_client()?;
    let body = s3::primitives::ByteStream::from(data);
    client
        .put_object()
        .bucket(BUCKET_NAME)
        .key(key)
        .body(body)
        .send()
        .await?;
    Ok(())
}
