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
    Setup(SetupRequestPayload),
    Start(StartRequestPayload),
    Retry(RetryRequestPayload),
}

/// 服务端响应类型的核心枚举。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerResponse {
    Ready,
    SetupSuccess(SetupResponse),
    StartSuccess(StartResponse),
    ChunkReadyForDownload {
        chunk_id: u64,
        passphrase_b64: String,
        retry: bool,
    },
    UploadCompleted,

    Error(String),
    InvalidRequest(String),
    NotFound(String),
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

// Setup 接口
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SetupRequestPayload {
    pub path: String,
    pub chunk_size: u64,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SetupResponse {
    pub file_name: String,
    pub file_size: u64,
    pub total_chunks: u64,
}

// Start 接口
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StartRequestPayload {
    pub resume_from_chunk_id: u64,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StartResponse {
    pub remaining_size: u64,
}

// Retry 接口
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RetryRequestPayload {
    pub chunk_id: u64,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RetryResponse {
    pub chunk_id: u64,
    pub passphrase_b64: String,
}
