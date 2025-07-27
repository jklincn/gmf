use anyhow::{Context, Ok, Result, anyhow};
use s3::creds::Credentials;
use s3::region::Region;
use s3::{Bucket, BucketConfiguration};
use std::env;
use tokio::sync::OnceCell;

pub const BUCKET_NAME: &str = "gmf";

static BUCKET_INSTANCE: OnceCell<Box<Bucket>> = OnceCell::const_new();

async fn get_bucket() -> Result<&'static Box<Bucket>> {
    BUCKET_INSTANCE
        .get_or_try_init(|| async {
            // 内部的初始化逻辑用一个内部 async block 包裹，方便统一处理错误
            let result: Result<Box<Bucket>> = async {
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

                let bucket_handle = Bucket::new(BUCKET_NAME, region.clone(), credentials.clone())
                    .context("创建 Bucket 客户端句柄失败")?;

                if bucket_handle.exists().await? {
                    Ok(bucket_handle)
                } else {
                    // 如果不存在，则创建
                    let config = BucketConfiguration::default();
                    let create_bucket_response =
                        Bucket::create(BUCKET_NAME, region, credentials, config).await?;
                    Ok(create_bucket_response.bucket)
                }
            }
            .await;
            result.context("Bucket 初始化逻辑失败")
        })
        .await
        .context("获取或初始化 Bucket 实例失败")
}

/// 将一个对象（字节数据）上传到存储桶。
/// TODO: 超时设计
pub async fn put_object(path: &str, content: &[u8]) -> Result<()> {
    let bucket = get_bucket().await?;

    // 1. 调用 put_object 并处理网络层/库层错误
    let response_data = bucket
        .put_object(path, content)
        .await
        .context(format!("向 S3 发送上传请求到 '{}' 时失败", path))?;

    // 2. 检查应用层（HTTP）的状态码
    let status_code = response_data.status_code();

    // S3 PUT 操作成功通常返回 200 OK
    if (200..300).contains(&status_code) {
        Ok(())
    } else {
        // 状态码不在 2xx 范围内，表示服务器端发生了错误
        // 从响应体中获取详细的错误信息（S3 通常会返回 XML 格式的错误描述）
        let error_body = String::from_utf8_lossy(response_data.bytes());
        Err(anyhow!(
            "上传对象到 '{}' 失败。状态码: {}. 错误信息: {}",
            path,
            status_code,
            error_body
        ))
    }
}
