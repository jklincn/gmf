// 警告修复：移除了未使用的导入 `use crate::r2;`
use crate::state::{AppState, TaskEvent, TaskMetadata};
use crate::worker::run_task;
use async_stream::stream;
use axum::{
    // `Json` 提取器和响应类型都需要
    Json,
    // 错误修复 1：从这里移除了 Path，避免与 std::path::Path 冲突
    extract::State, 
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    // 单独导入 axum::extract::Path，并在需要时使用
    extract::Path as AxumPath,
};
use serde::{Deserialize, Serialize};
// 错误修复 2：导入 Digest trait 以使用 .new(), .update(), .finalize()
use sha2::{Digest, Sha256}; 
// 潜在错误修复：导入 hex crate
use hex;
use std::io::{BufReader, Read};
// 错误修复 1 的另一种方式：现在可以直接使用 Path 而不会混淆了
use std::{convert::Infallible, fs::File, path::{Path, PathBuf}}; 
use tokio::sync::broadcast;
use tracing::{error, info, instrument, warn};

pub const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10MB

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

#[derive(Deserialize, Debug)]
pub struct SetupPayload {
    path: String,
}

#[derive(Serialize)]
pub struct SetupResponse {
    filename: String,
    size: u64,
    sha256: String,
}

fn calculate_sha256(path: &Path) -> Result<String, std::io::Error> {
    let input = File::open(path)?;
    let mut reader = BufReader::new(input);
    let mut hasher = Sha256::new(); 
    let mut buffer = [0; 1024 * 4];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }

    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

#[instrument(skip(state, payload), fields(path = %payload.path))]
pub async fn setup(
    State(state): State<AppState>,
    Json(payload): Json<SetupPayload>,
) -> Result<impl IntoResponse, Response> {
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
    let total_chunks = (source_size as f64 / CHUNK_SIZE as f64).ceil() as u32;

    // 计算文件的 SHA256 哈希
    let source_sha256 = match calculate_sha256(&source_path) {
        Ok(hash) => hash,
        Err(e) => {
            let msg = format!("计算文件 '{}' 的 SHA256 哈希失败: {}", source_path.display(), e);
            return Err((StatusCode::INTERNAL_SERVER_ERROR, msg).into_response());
        }
    };

    // 假设 TaskMetadata 结构体也已更新
    let task_metadata = TaskMetadata {
        source_path,
        source_filename: source_filename.clone(),
        source_size,
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
            total_chunks = task_metadata.total_chunks,
            "任务元数据已创建，正在初始化全局状态"
        );
        context.metadata = Some(task_metadata);
    }

    // 构造成功响应
    let response_data = SetupResponse {
        filename: source_filename,
        size: source_size,
        sha256: source_sha256,
    };
    
    // 使用 axum::Json 包装器返回 JSON
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

// #[instrument(skip(state), fields(chunk_id = chunk_id))]
// // 错误修复 1：使用重命名后的 AxumPath
// pub async fn acknowledge(State(state): State<AppState>, AxumPath(chunk_id): AxumPath<u32>) -> StatusCode {
//     info!("接收到分块确认");

//     let context = state.0.lock().unwrap();

//     if let Some(sender) = &context.event_sender {
//         let event = TaskEvent::ChunkAcknowledged { chunk_id };

//         match sender.send(event) {
//             Ok(receiver_count) => {
//                 if receiver_count > 0 {
//                     info!(
//                         "已将确认事件广播给 {} 个活动的 worker/listener",
//                         receiver_count
//                     );
//                 } else {
//                     warn!("确认事件已发送，但没有活动的接收者。可能对应的 worker 已经超时。");
//                 }
//                 StatusCode::OK
//             }
//             Err(_) => {
//                 error!("无法广播确认事件，任务可能已经终止。");
//                 StatusCode::INTERNAL_SERVER_ERROR
//             }
//         }
//     } else {
//         warn!("收到确认请求，但任务当前未在运行状态 (event_sender is None)。");
//         StatusCode::BAD_REQUEST
//     }
// }
