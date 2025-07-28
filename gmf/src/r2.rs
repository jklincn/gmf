use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use log::info;
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

            let bucket = Bucket::new(BUCKET_NAME, region, credentials)
                .context("初始化 S3 Bucket 实例失败")?;

            let result: Result<Box<Bucket>> = Ok(bucket);
            result
        })
        .await
        .context("获取或初始化 Bucket 实例失败")
}

pub async fn list_all_object_keys() -> Result<Vec<String>> {
    let bucket = get_bucket().await?;

    let list_results = bucket
        .list("/".to_string(), None)
        .await
        .context("列出存储桶中的对象失败")?;

    let all_keys: Vec<String> = list_results
        .into_iter()
        .flat_map(|page| page.contents)
        .map(|object| object.key)
        .collect();

    Ok(all_keys)
}

/// 从存储桶下载一个对象
pub async fn get_object(path: &str) -> Result<Bytes> {
    let bucket = get_bucket().await?;

    let response = bucket
        .get_object(path)
        .await
        .context(format!("从存储桶下载对象 '{path}' 的请求失败"))?;

    if response.status_code() != 200 {
        return Err(anyhow!(
            "下载对象 '{}' 失败，API 返回非成功状态码: {}, 响应体: {:?}",
            path,
            response.status_code(),
            String::from_utf8_lossy(response.bytes())
        ));
    }

    Ok(response.into_bytes())
}

/// 从存储桶中删除一个对象
pub async fn delete_object(path: &str) -> Result<()> {
    let bucket = get_bucket().await?;
    let response = bucket
        .delete_object(path)
        .await
        .context(format!("从存储桶删除对象 '{path}' 的请求失败"))?;

    if response.status_code() != 204 {
        return Err(anyhow!(
            "删除对象 '{}' 失败，API 返回非成功状态码: {}, 响应体: {:?}",
            path,
            response.status_code(),
            String::from_utf8_lossy(response.bytes())
        ));
    }

    Ok(())
}



/// 清空存储桶中的所有对象
async fn clear_bucket() -> Result<()> {
    let bucket = get_bucket().await?;

    // 获取存储桶中的所有对象
    let list_results = bucket
        .list("/".to_string(), None)
        .await
        .context("列出存储桶中的对象失败")?;

    let all_keys: Vec<String> = list_results
        .into_iter()
        .flat_map(|page| page.contents)
        .map(|object| object.key)
        .collect();

    if all_keys.is_empty() {
        return Ok(());
    }

    let delete_futures = stream::iter(all_keys).map(|key| async move {
        match delete_object(&key).await {
            Ok(_) => Ok(key),
            Err(e) => Err((key, e)),
        }
    });

    let results: Vec<Result<String, (String, anyhow::Error)>> =
        delete_futures.buffer_unordered(10).collect().await;

    let errors: Vec<(String, anyhow::Error)> =
        results.into_iter().filter_map(Result::err).collect();

    if !errors.is_empty() {
        let error_details = errors
            .iter()
            .map(|(key, e)| format!("  - 删除 '{}' 失败: {:?}", key, e))
            .collect::<Vec<String>>()
            .join("\n");

        return Err(anyhow!(
            "清空存储桶时发生 {} 个错误:\n{}",
            errors.len(),
            error_details
        ));
    }

    Ok(())
}

pub async fn delete_bucket() -> Result<()> {
    // 1. 清空前检查
    info!("--- 步骤 1: 准备清空存储桶 '{}' ---", BUCKET_NAME);
    let keys_before = list_all_object_keys().await.context("清空前列出对象失败")?;
    if keys_before.is_empty() {
        info!("[信息] 清空前：存储桶已为空。");
    } else {
        info!("[信息] 清空前，存储桶中有 {} 个对象：", keys_before.len());
        // 只打印前 10 个以防列表过长
        for key in keys_before.iter().take(10) {
            info!("  - {}", key);
        }
        if keys_before.len() > 10 {
            info!("  ... (及其他 {} 个对象)", keys_before.len() - 10);
        }
    }

    // 2. 执行清空操作
    info!("\n--- 步骤 2: 正在执行清空操作... ---");
    match clear_bucket().await {
        Ok(_) => info!("[成功] 清空操作执行完毕。"),
        Err(e) => {
            // 如果 clear_bucket 本身就报错了，直接返回
            info!("[错误] 清空操作失败。");
            return Err(e).context("清空存储桶失败");
        }
    }
    
    // 3. 清空后再次检查
    info!("\n--- 步骤 3: 检查清空后存储桶的状态 ---");
    let keys_after = list_all_object_keys().await.context("清空后列出对象失败")?;
    if keys_after.is_empty() {
        info!("[成功] 清空后：存储桶已成功变为空。");
    } else {
        // 这是诊断的关键！
        info!("[警告] 清空后，存储桶中仍有 {} 个对象：", keys_after.len());
        for key in &keys_after {
            info!("  - {}", key);
        }
        info!("[诊断] 这很可能是导致删除失败 (409 Conflict) 的原因。");
        info!("[诊断] 请检查您的 `clear_bucket` 函数是否处理了对象版本或未完成的分块上传。");
    }

    // 4. 尝试删除存储桶
    info!("\n--- 步骤 4: 正在尝试删除存储桶 '{}'... ---", BUCKET_NAME);
    let bucket = get_bucket().await?;
    let status_code = bucket
        .delete()
        .await
        .context(format!("删除存储桶 '{BUCKET_NAME}' 的请求失败"))?;

    if status_code == 204 {
        info!("\n[最终成功] 存储桶 '{}' 已被成功删除！", BUCKET_NAME);
        Ok(())
    } else {
        info!("\n[最终失败] 删除存储桶失败。");
        Err(anyhow!(
            "删除存储桶 '{}' 失败，API 返回非预期的状态码: {} (预期为 204)",
            BUCKET_NAME,
            status_code
        ))
    }
}