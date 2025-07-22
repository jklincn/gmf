use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use r2::{ChunkInfo, TaskEvent};

// --- 状态枚举 ---
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ChunkStatus {
    Pending,          // 待处理
    Uploading,        // 正在上传
    ReadyForDownload, // 已就绪，等待客户端下载
    Acknowledged,     // 客户端已确认收到
    Deleting,         // 正在删除
    Completed,        // 已完成
    Failed(String),   // 失败
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChunkState {
    pub info: ChunkInfo,
    pub status: ChunkStatus,
    pub remote_path: Option<String>,
}

#[derive(Debug)]
pub struct TaskContext {
    pub path: Option<PathBuf>,
    pub chunks: HashMap<u32, ChunkState>,
    pub event_sender: Option<broadcast::Sender<TaskEvent>>,
}

impl Default for TaskContext {
    fn default() -> Self {
        Self {
            path: None,
            chunks: HashMap::new(),
            event_sender: None,
        }
    }
}

#[derive(Clone)]
pub struct AppState(pub Arc<Mutex<TaskContext>>);

impl AppState {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(TaskContext::default())))
    }
}
