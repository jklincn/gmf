use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::File, io, path::Path};

pub const NONCE_SIZE: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum TaskEvent {
    ProcessingStart,
    ChunkReadyForDownload {
        chunk_id: u64,
        passphrase_b64: String,
    },
    ChunkAcknowledged {
        chunk_id: u64,
    },
    TaskCompleted,
    Error {
        message: String,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SetupRequestPayload {
    pub path: String,
    pub chunk_size: u64,
    pub concurrency: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SetupResponse {
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub total_chunks: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StartRequestPayload {
    pub resume_from_chunk_id: u64,
}

pub fn format_size(size: u64) -> String {
    if size == 0 {
        return "0.00 B".to_string();
    }

    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut size = size as f64;
    let mut unit = 0;

    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }

    format!("{:.2} {}", size, UNITS[unit])
}

/// 计算指定文件的 SHA256 哈希值
pub fn calc_sha256(path: &Path) -> Result<String> {
    let mut input = File::open(path)
        .with_context(|| format!("打开文件 '{}' 失败用于计算 SHA256", path.display()))?;
    let mut hasher = Sha256::new();

    io::copy(&mut input, &mut hasher)
        .with_context(|| format!("读取文件 '{}' 内容失败用于计算 SHA256", path.display()))?;

    let hash = hasher.finalize();

    Ok(hex::encode(hash))
}

/// 获取指定文件的大小
pub fn file_size(path: &Path) -> Result<u64> {
    let file =
        File::open(path).with_context(|| format!("无法打开文件 '{}' 获取大小", path.display()))?;

    let metadata = file
        .metadata()
        .with_context(|| format!("无法读取文件 '{}' 的元数据", path.display()))?;

    Ok(metadata.len())
}
