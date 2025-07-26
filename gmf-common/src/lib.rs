use serde::{Deserialize, Serialize};

pub const NONCE_SIZE: usize = 12;

#[derive(Serialize, Deserialize, Debug)]
pub struct SetupRequestPayload {
    pub path: String,
    pub chunk_size: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SetupResponse {
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub total_chunks: u32,
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
