use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use anyhow::Result;

// ===================================================================
// 1. 核心消息结构 (Core Message Structures)
// ===================================================================

/// 顶层消息包装器。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_type", content = "payload")]
pub enum Message {
    /// 客户端请求。
    Request(ClientRequest),
    /// 服务端响应（成功和错误都被视为一种响应）。
    Response(ServerResponse),
    /// 服务端调试信息。
    DebugLog(Vec<DebugMessage>),
}

/// 客户端请求的枚举。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ClientRequest {
    Setup(SetupRequestPayload),
    Start(StartRequestPayload),
}

/// 服务端响应类型的核心枚举。
/// 【核心修改】现在包含了成功和失败的各种情况。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerResponse {
    // 正在运行
    Ready,
    // --- 成功响应 ---
    SetupSuccess(SetupResponse),
    StartSuccess,
    ChunkReadyForDownload {
        chunk_id: u64,
        passphrase_b64: String,
    },
    UploadCompleted,

    // --- 【新增】错误响应 ---
    /// 通用错误，当错误不属于特定类别时使用。
    Error(String),
    /// 当请求本身无效时（如缺少字段、格式错误）。
    InvalidRequest(String),
    /// 当在错误的状态下执行操作时（如未Setup就Start）。
    InvalidState(String),
    /// 当请求的文件或资源找不到时。
    NotFound(String),
    FatalError(String),
}

// ... 调试信息结构保持不变 ...
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DebugLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugMessage {
    pub level: DebugLevel,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<HashMap<String, String>>,
}

// ===================================================================
// 2. 载荷数据 (Payload Data)
// ===================================================================

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

// ===================================================================
// 3. 实现与辅助函数 (Implementations & Helpers)
// ===================================================================

// --- Message 创建助手 ---
impl Message {
    pub fn create_debug_log(log: Vec<DebugMessage>) -> Self {
        Message::DebugLog(log)
    }
    pub fn create_single_debug(level: DebugLevel, message: impl Into<String>) -> Self {
        Message::DebugLog(vec![DebugMessage {
            level,
            message: message.into(),
            context: None,
        }])
    }
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

// --- 序列化/反序列化助手
impl Message {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}
