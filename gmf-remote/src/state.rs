use r2::TaskEvent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum ChunkProcessingStatus {
    Completed,                 // 成功完成所有步骤
    Failed { reason: String }, // 在某个步骤失败
}

// ChunkState 现在只记录最终结果
#[derive(Debug, Clone, Serialize)]
pub struct ChunkState {
    pub id: u32,
    pub status: ChunkProcessingStatus,
}

/// 存储任务的静态元数据，这些信息在任务开始后不会改变。
#[derive(Debug, Clone, Serialize)]
pub struct TaskMetadata {
    pub source_path: PathBuf,
    pub source_filename: String,
    pub source_size: u64,
    pub total_chunks: u32,
}

/// 整个任务的上下文，包含所有状态和通信渠道。
#[derive(Debug)]
pub struct TaskContext {
    pub metadata: Option<TaskMetadata>,
    pub chunks: HashMap<u32, ChunkState>,
    pub event_sender: Option<broadcast::Sender<TaskEvent>>,
}

impl TaskContext {
    pub fn new() -> Self {
        Self {
            metadata: None,
            chunks: HashMap::new(),
            event_sender: None,
        }
    }
}

#[derive(Clone)]
pub struct AppState(pub Arc<Mutex<TaskContext>>);

impl AppState {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(TaskContext::new())))
    }
}
