use serde::{Deserialize, Serialize};

pub const NONCE_SIZE: usize = 12;

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

#[derive(Serialize, Deserialize, Debug)]
pub struct SetupRequestPayload {
    pub path: String,
    pub chunk_size: usize,
    pub concurrency: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SetupResponse {
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub total_chunks: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StartRequestPayload {
    // 服务端从哪个 chunk_id (1-based) 开始发送
    pub resume_from_chunk_id: u32,
}

pub fn format_size(mut size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut unit = 0;
    while size >= 1024 && unit < UNITS.len() - 1 {
        size /= 1024;
        unit += 1;
    }
    format!("{} {}", size, UNITS[unit])
}