use anyhow::{Context, Result};
use aws_config::{self, BehaviorVersion, Region};
use aws_sdk_s3 as s3;
use aws_sdk_s3::config::Credentials;
use bytes::Bytes;
use std::env;
use tokio::sync::OnceCell;

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

    let (endpoint, access_key_id, secret_access_key) = if let Some(config) = config_override {
        // 传参初始化
        (
            config.endpoint,
            config.access_key_id,
            config.secret_access_key,
        )
    } else {
        // 环境变量初始化
        let endpoint = env::var("ENDPOINT").context("环境变量 'ENDPOINT' 未设置")?;
        let access_key_id = env::var("ACCESS_KEY_ID").context("环境变量 'ACCESS_KEY_ID' 未设置")?;
        let secret_access_key =
            env::var("SECRET_ACCESS_KEY").context("环境变量 'SECRET_ACCESS_KEY' 未设置")?;
        (endpoint, access_key_id, secret_access_key)
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
        .load()
        .await;

    let client = s3::Client::new(&config);

    S3_CLIENT
        .set(client)
        .map_err(|_| anyhow::anyhow!("设置 S3 客户端失败，可能已被其他线程初始化"))?;
    Ok(())
}

fn get_s3_client() -> Result<&'static s3::Client> {
    S3_CLIENT
        .get()
        .context("S3 客户端尚未初始化。请在程序启动时调用 init_s3_client。")
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

pub async fn delete_bucket() -> Result<()> {
    let client = get_s3_client()?;

    let objects = list_objects().await?;
    if !objects.is_empty() {
        delete_objects(objects).await?;
    }

    client.delete_bucket().bucket(BUCKET_NAME).send().await?;
    Ok(())
}

pub async fn get_object(key: &str) -> Result<Bytes> {
    let client = get_s3_client()?;
    let resp = client
        .get_object()
        .bucket(BUCKET_NAME)
        .key(key)
        .send()
        .await?;
    Ok(resp.body.collect().await?.into_bytes())
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
