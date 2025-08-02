use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 顶层消息包装器。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_type", content = "payload")]
pub enum Message {
    /// 客户端请求。
    Request(ClientRequest),
    /// 服务端响应（成功和错误都被视为一种响应）。
    Response(ServerResponse),
}

/// 客户端请求的枚举。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ClientRequest {
    Setup(SetupRequestPayload),
    Start(StartRequestPayload),
}

/// 服务端响应类型的核心枚举。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerResponse {
    Ready,
    SetupSuccess(SetupResponse),
    StartSuccess,
    ChunkReadyForDownload {
        chunk_id: u64,
        passphrase_b64: String,
    },
    UploadCompleted,

    Error(String),
    InvalidRequest(String),
    NotFound(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SetupRequestPayload {
    pub path: String,
    pub chunk_size: u64,
    pub concurrency: u64,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SetupResponse {
    pub file_name: String,
    pub file_size: u64,
    pub total_chunks: u64,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StartRequestPayload {
    pub resume_from_chunk_id: u64,
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
