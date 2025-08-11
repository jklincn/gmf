use anyhow::Result;
use serde::{Deserialize, Serialize};

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
    Start { resume_from_chunk_id: u64 },
    Retry { chunk_id: u64 },
}

/// 服务端响应类型的核心枚举。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerResponse {
    Ready,
    SetupSuccess {
        file_name: String,
        file_size: u64,
        total_chunks: u64,
    },
    StartSuccess {
        remaining_size: u64,
    },
    ChunkReadyForDownload {
        chunk_id: u64,
        passphrase_b64: String,
        retry: bool,
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
