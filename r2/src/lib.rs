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
pub const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10MB
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum TaskEvent {
    ProcessingStart,
    ChunkReadyForDownload {
        chunk_id: u32,
        passphrase_b64: String,
    },
    ChunkAcknowledged {
        chunk_id: u32,
    },
    TaskCompleted,
    Error {
        message: String,
    },
}

// === 解密文件 ===
pub fn decrypt_chunk(key_bytes: &[u8; 32], encrypted_data: &[u8]) -> anyhow::Result<Vec<u8>> {
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