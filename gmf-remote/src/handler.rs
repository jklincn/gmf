use crate::state::{AppState, TaskMetadata};
use crate::worker::run_task;
use async_stream::stream;
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use gmf_common::{SetupRequestPayload, SetupResponse, TaskEvent};
use hex;
use sha2::{Digest, Sha256};
use std::io;
use std::{
    convert::Infallible,
    fs::File,
    path::{Path, PathBuf},
};
use tokio::sync::broadcast;
use tracing::{error, info, instrument, warn};

#[instrument]
pub async fn healthy() -> StatusCode {
    info!("健康检查接口被调用");
    StatusCode::OK
}

fn format_size(mut size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut unit = 0;
    while size >= 1024 && unit < UNITS.len() - 1 {
        size /= 1024;
        unit += 1;
    }
    format!("{} {}", size, UNITS[unit])
}

fn calculate_sha256(path: &Path) -> Result<String, std::io::Error> {
    let mut input = File::open(path)?;
    let mut hasher = Sha256::new();

    io::copy(&mut input, &mut hasher)?;

    let hash = hasher.finalize();

    Ok(hex::encode(hash))
}

#[instrument(skip(state, payload), fields(path = %payload.path, chunk_size = payload.chunk_size))]
pub async fn setup(
    State(state): State<AppState>,
    Json(payload): Json<SetupRequestPayload>,
) -> Result<impl IntoResponse, Response> {
    if payload.chunk_size == 0 {
        let msg = "chunk_size 不能为 0".to_string();
        return Err((StatusCode::BAD_REQUEST, msg).into_response());
    }

    // 路径处理和验证
    let source_path: PathBuf = match shellexpand::tilde(&payload.path).to_string().parse() {
        Ok(p) => p,
        Err(_) => {
            let msg = format!("提供的路径 '{}' 无效", payload.path);
            return Err((StatusCode::BAD_REQUEST, msg).into_response());
        }
    };

    let source_filename = match source_path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.to_string(),
        None => {
            let msg = format!("无法从路径 '{}' 中提取文件名", source_path.display());
            return Err((StatusCode::BAD_REQUEST, msg).into_response());
        }
    };

    // 文件元数据
    let file_metadata = match File::open(&source_path) {
        Ok(file) => match file.metadata() {
            Ok(meta) => meta,
            Err(e) => {
                let msg = format!("无法获取文件 '{}' 的元数据: {}", source_path.display(), e);
                return Err((StatusCode::INTERNAL_SERVER_ERROR, msg).into_response());
            }
        },
        Err(e) => {
            let msg = format!("无法打开文件 `{}`: {}", source_path.display(), e);
            return Err((StatusCode::BAD_REQUEST, msg).into_response());
        }
    };

    if !file_metadata.is_file() {
        let msg = format!("提供的路径 '{}' 不是一个文件", source_path.display());
        return Err((StatusCode::BAD_REQUEST, msg).into_response());
    }

    let source_size = file_metadata.len();

    let total_chunks = (source_size as f64 / payload.chunk_size as f64).ceil() as u32;

    // 计算文件的 SHA256 哈希
    let source_sha256 = match calculate_sha256(&source_path) {
        Ok(hash) => hash,
        Err(e) => {
            let msg = format!(
                "计算文件 '{}' 的 SHA256 哈希失败: {}",
                source_path.display(),
                e
            );
            return Err((StatusCode::INTERNAL_SERVER_ERROR, msg).into_response());
        }
    };

    // 初始化 TaskMetadata
    let task_metadata = TaskMetadata {
        source_path,
        source_filename: source_filename.clone(),
        source_size,
        chunk_size: payload.chunk_size,
        total_chunks,
        sha256: source_sha256.clone(),
    };

    // 更新全局状态
    {
        let mut context = state.0.lock().unwrap();

        info!(
            file = %task_metadata.source_filename,
            size = %format_size(task_metadata.source_size),
            sha256 = %task_metadata.sha256,
            chunk_size = %format_size(task_metadata.chunk_size.try_into().unwrap()), // 记录分块大小
            total_chunks = task_metadata.total_chunks,
            "任务元数据已创建，正在初始化全局状态"
        );
        context.metadata = Some(task_metadata);
    }

    let response_data = SetupResponse {
        filename: source_filename,
        size: source_size,
        sha256: source_sha256,
        total_chunks,
    };

    Ok((StatusCode::OK, Json(response_data)))
}

#[instrument(skip_all, name = "start_task_sse_stream")]
pub async fn start(State(state): State<AppState>) -> impl IntoResponse {
    let (tx, mut rx) = broadcast::channel::<TaskEvent>(128);
    {
        let mut context = state.0.lock().unwrap();
        context.event_sender = Some(tx.clone());
    }

    tokio::spawn(run_task(state.clone()));

    let event_stream = stream! {
        loop {
            match rx.recv().await {
                Ok(task_event) => {
                    let is_final_event = matches!(
                        task_event,
                        TaskEvent::TaskCompleted | TaskEvent::Error { .. }
                    );

                    let json_data = match serde_json::to_string(&task_event) {
                        Ok(json) => json,
                        Err(e) => {
                            error!(error = %e, "无法序列化 TaskEvent 为 JSON");
                            let err_event = TaskEvent::Error { message: "Internal serialization error".to_string() };
                            serde_json::to_string(&err_event).unwrap()
                        }
                    };

                    yield Ok::<Event, Infallible>(Event::default().data(json_data));

                    if is_final_event {
                        info!("检测到最终事件，关闭 SSE 流。");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(count = n, "SSE 客户端接收滞后，丢失了 {} 个事件。", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("广播通道已关闭，SSE 流正常结束。");
                    break;
                }
            }
        }
    };

    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
