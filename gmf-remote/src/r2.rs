use anyhow::{Context, Result};
use s3::Bucket;
use s3::creds::Credentials;
use s3::region::Region;
use std::env;
use tokio::sync::OnceCell;

pub const BUCKET_NAME: &str = "gmf";

static BUCKET_INSTANCE: OnceCell<Box<Bucket>> = OnceCell::const_new();

async fn get_bucket() -> Result<&'static Box<Bucket>> {
    BUCKET_INSTANCE
        .get_or_try_init(|| async {
            let endpoint = env::var("ENDPOINT").context("环境变量 'ENDPOINT' 未设置")?;

            let region = Region::Custom {
                region: "auto".to_string(),
                endpoint,
            };

            let credentials = Credentials::from_env_specific(
                Some("ACCESS_KEY_ID"),
                Some("SECRET_ACCESS_KEY"),
                None,
                None,
            )
            .context("无法从环境变量加载凭证")?;

            let bucket = Bucket::new(BUCKET_NAME, region, credentials)?;

            let result: Result<Box<Bucket>> = Ok(bucket);
            result
        })
        .await
        .context("获取或初始化 Bucket 实例失败")
}

/// 将一个对象（字节数据）上传到存储桶。
pub async fn put_object(path: &str, content: &[u8]) -> Result<()> {
    let bucket = get_bucket().await?;

    bucket
        .put_object(path, content)
        .await
        .context(format!("上传对象到 '{path}' 失败"))?;

    Ok(())
}
