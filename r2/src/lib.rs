use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result, bail};
use aws_sdk_s3 as s3;
use aws_sdk_s3::primitives::ByteStream;
use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

const NONCE_SIZE: usize = 12;
const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10MB
const BUCKETNAME: &str = "gmf";

/// 分块信息结构体
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChunkInfo {
    pub id: u32,                // 分块序号（从 1 开始）
    pub local_path: PathBuf,    // 远程最终文件名
    pub passphrase_b64: String, // Base64-encoded 随机口令
    pub sha256: String,         // 加密后文件哈希
    pub size: u64,              // 加密后大小（字节）
}

/// 整个文件的清单
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Manifest {
    pub filename: PathBuf,
    pub total_size: u64,
    pub chunk_size: usize,
    pub total_chunks: u32,
    pub chunks: Vec<ChunkInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "data")]
pub enum TaskEvent {
    ProcessingStart,
    SplitComplete { manifest: Manifest },
    ChunkReadyForDownload { chunk_id: u32, remote_path: String },
    ChunkAcknowledged { chunk_id: u32 },
    TaskCompleted,
    Error { message: String },
}

fn generate_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    key
}

// === 加密文件 ===
fn encrypt_chunk(key_bytes: &[u8; 32], input_data: &[u8]) -> anyhow::Result<Vec<u8>> {
    // 生成随机 nonce（每个文件都唯一）
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, input_data)
        .map_err(|e| anyhow::anyhow!("加密失败: {:?}", e))?;

    // 输出格式：nonce || ciphertext
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes[..]);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

// === 解密文件 ===
fn decrypt_chunk(key_bytes: &[u8; 32], encrypted_data: &[u8]) -> anyhow::Result<Vec<u8>> {
    if encrypted_data.len() < NONCE_SIZE {
        anyhow::bail!("输入文件太短，无法包含有效 nonce");
    }

    let (nonce_bytes, ciphertext) = encrypted_data.split_at(NONCE_SIZE);

    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("解密失败: {:?}", e))?;
    Ok(plaintext)
}

/// 对文件分块并对每个分块进行加密
pub async fn split_and_encrypt(input_path: impl AsRef<Path>) -> anyhow::Result<Manifest> {
    let input_path: &Path = input_path.as_ref();
    let input_file = File::open(input_path)?;
    let file_size = input_file.metadata()?.len();
    let mut reader = BufReader::new(input_file);

    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut chunk_index = 0;
    let mut chunks_info = Vec::new();

    // 创建一个临时目录用于存储分块
    let temp_dir = env::temp_dir();
    let mut rng = OsRng;
    let random_id = rng.next_u64();
    let work_dir_name = format!("gmf-{}", random_id);
    let work_dir = temp_dir.join(&work_dir_name);
    fs::create_dir_all(&work_dir)?;

    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        chunk_index += 1;
        let current_chunk_data = &buffer[..n];

        // 1. 为分块生成随机加密密钥
        let key = generate_key();
        let passphrase_b64 = general_purpose::STANDARD.encode(key);

        // 2. 加密分块
        let encrypted_data = encrypt_chunk(&key, current_chunk_data)?;

        // 3. 计算哈希和大小
        let sha256 = format!("{:x}", Sha256::digest(&encrypted_data));
        let size = encrypted_data.len() as u64;

        // 4. 将加密后的分块写入随机子目录
        let chunk_basename = format!("gmf.part{}", chunk_index);
        let chunk_path = work_dir.join(chunk_basename);
        fs::write(&chunk_path, &encrypted_data)?;

        // 5. 收集分块信息
        chunks_info.push(ChunkInfo {
            id: chunk_index,
            local_path: chunk_path,
            passphrase_b64,
            sha256,
            size,
        });
    }

    Ok(Manifest {
        filename: input_path.to_path_buf(),
        total_size: file_size,
        chunk_size: CHUNK_SIZE,
        total_chunks: chunk_index,
        chunks: chunks_info,
    })
}

/// 对文件解密并合并
pub fn decrypt_and_merge(manifest: &Manifest, output_path: impl AsRef<Path>) -> anyhow::Result<()> {
    // 使用临时路径以便后续重命名
    let output_path_ref = output_path.as_ref();
    let temp_output_path = output_path_ref.to_path_buf();
    let mut output_file = File::create(&temp_output_path)?;
    let temp_dir = env::temp_dir();

    // 确保分块按顺序处理
    let mut sorted_chunks = manifest.chunks.clone();
    sorted_chunks.sort_by_key(|c| c.id);

    for chunk_info in sorted_chunks {
        // 1. 读取加密的分块文件
        let chunk_path = temp_dir.join(&chunk_info.local_path);
        if !chunk_path.exists() {
            anyhow::bail!("分块文件不存在: {:?}", chunk_path);
        }
        let encrypted_data = fs::read(chunk_path)?;

        // 2. 验证文件大小和哈希
        if encrypted_data.len() as u64 != chunk_info.size {
            anyhow::bail!(
                "分块 {} 大小不匹配. 预期: {}, 实际: {}",
                chunk_info.id,
                chunk_info.size,
                encrypted_data.len()
            );
        }
        let sha256 = format!("{:x}", Sha256::digest(&encrypted_data));
        if sha256 != chunk_info.sha256 {
            anyhow::bail!("分块 {} SHA256校验和不匹配.", chunk_info.id);
        }

        // 3. 解码密钥并解密
        let key = general_purpose::STANDARD.decode(&chunk_info.passphrase_b64)?;
        let decrypted_data = decrypt_chunk(key.as_slice().try_into()?, &encrypted_data)?;

        // 4. 写入输出文件
        output_file.write_all(&decrypted_data)?;
    }

    // 恢复原始文件名并重命名
    let original_os_name = manifest
        .filename
        .file_name()
        .expect("原始文件名丢失")
        .to_os_string();
    let restored_path = temp_output_path.with_file_name(&original_os_name);
    fs::rename(&temp_output_path, &restored_path)?;
    println!("解密完成，文件已保存为: {:?}", restored_path);

    Ok(())
}

pub async fn get_config<'a>(
    cfg: Option<(&'a str, &'a str, &'a str)>,
) -> anyhow::Result<s3::Client> {
    let endpoint;
    let access_key_id;
    let secret_access_key;
    if let Some((ep, akid, sak)) = cfg {
        endpoint = ep.to_string();
        access_key_id = akid.to_string();
        secret_access_key = sak.to_string();
    } else {
        endpoint = env::var("ENDPOINT").unwrap();
        access_key_id = env::var("ACCESS_KEY_ID").unwrap();
        secret_access_key = env::var("SECRET_ACCESS_KEY").unwrap();
    }
    let config = aws_config::from_env()
        .endpoint_url(endpoint)
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            access_key_id,
            secret_access_key,
            None,
            None,
            "R2",
        ))
        .region("auto")
        .load()
        .await;

    let client = s3::Client::new(&config);
    Ok(client)
}

pub async fn create_bucket(client: &s3::Client) -> Result<()> {
    client
        .create_bucket()
        .bucket(BUCKETNAME)
        .create_bucket_configuration(s3::types::CreateBucketConfiguration::builder().build())
        .send()
        .await
        .context(format!("向 S3 发送创建 Bucket '{}' 的请求失败", BUCKETNAME))?;

    let mut buckets = client.list_buckets().into_paginator().send();

    while let Some(page) = buckets.next().await {
        let output = page.context("从 S3 获取 Bucket 列表页面失败")?;
        for bucket in output.buckets() {
            if bucket.name.as_deref() == Some(BUCKETNAME) {
                return Ok(());
            }
        }
    }

    bail!("Bucket '{}' 创建后验证失败，未在列表中找到。", BUCKETNAME);
}

pub async fn delete_bucket(client: &s3::Client) -> Result<()> {
    client
        .delete_bucket()
        .bucket(BUCKETNAME)
        .send()
        .await
        .context(format!("删除 Bucket '{}' 失败", BUCKETNAME))?;

    Ok(())
}

pub async fn upload_object(client: &s3::Client, data: &[u8], key: &str) -> Result<()> {
    let body = ByteStream::from(data.to_vec());
    client
        .put_object()
        .bucket(BUCKETNAME)
        .key(key)
        .body(body)
        .send()
        .await
        .context(format!("上传内存数据到 S3 对象 '{}' 失败", key))?;
    Ok(())
}
