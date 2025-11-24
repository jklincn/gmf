use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::sync::{Mutex, MutexGuard};

use gmf_common::chunk_bitmap::ChunkBitmap;

pub struct TaskMetadata {
    file_path: PathBuf,
    file_size: u64,
    chunk_size: u64,
    total_chunks: u64,
}

impl TaskMetadata {
    pub fn new(file_path: PathBuf, file_size: u64, chunk_size: u64, total_chunks: u64) -> Self {
        TaskMetadata {
            file_path,
            file_size,
            chunk_size,
            total_chunks,
        }
    }

    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    pub fn total_chunks(&self) -> u64 {
        self.total_chunks
    }
}

pub fn init_metadata(metadata: TaskMetadata) {
    if METADATA.set(metadata).is_err() {
        panic!("Global metadata 已经被初始化过了!");
    }
}

pub fn global_metadata() -> &'static TaskMetadata {
    METADATA.get().expect("CONFIG not initialized")
}

pub fn init_chunk_bitmap(chunk_bitmap: ChunkBitmap) {
    if CHUNK_BITMAP.set(Mutex::new(chunk_bitmap)).is_err() {
        panic!("Global bitmap 已经被初始化过了!");
    }
}

pub async fn global_chunk_bitmap() -> MutexGuard<'static, ChunkBitmap> {
    CHUNK_BITMAP
        .get()
        .expect("Global bitmap 尚未初始化! 请先调用 init_chunk_bitmap")
        .lock()
        .await
}

static METADATA: OnceLock<TaskMetadata> = OnceLock::new();
static CHUNK_BITMAP: OnceLock<Mutex<ChunkBitmap>> = OnceLock::new();
