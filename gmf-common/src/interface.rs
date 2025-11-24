use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::chunk_bitmap::ChunkBitmap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_type", content = "payload")]
pub enum Message {
    Request(ClientRequest),
    Response(ServerResponse),
}

/// 客户端请求的枚举。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ClientRequest {
    Setup { path: String, chunk_size: u64 },
    Start { chunk_bitmap: ChunkBitmap },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupResponse {
    pub file_name: String,
    pub file_size: u64,
    pub total_chunks: u64,
    pub xxh3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartResponse {
    pub remaining_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DownloadEvent {
    ChunkReady {
        chunk_id: u64,
        passphrase_b64: String,
    },
    UploadCompleted,
}

/// 服务端响应类型的核心枚举。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerResponse {
    Heartbeat,
    SetupSuccess(SetupResponse),
    StartSuccess(StartResponse),
    ChunkReadyForDownload {
        chunk_id: u64,
        passphrase_b64: String,
    },
    UploadCompleted,
    Error(String),
}

impl From<ServerResponse> for Message {
    fn from(response: ServerResponse) -> Self {
        Message::Response(response)
    }
}

impl From<ClientRequest> for Message {
    fn from(request: ClientRequest) -> Self {
        Message::Request(request)
    }
}

impl Message {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}
