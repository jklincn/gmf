use crate::state::{AppState, TaskMetadata};
use crate::worker::run_task;
use async_stream::stream;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use r2::{CHUNK_SIZE, TaskEvent};
use serde::Deserialize;
use std::{convert::Infallible, fs::File, path::PathBuf};
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

#[derive(Deserialize, Debug)]
pub struct SetupPayload {
    path: String,
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

    // 创建 TaskMetadata 实例
    let task_metadata = TaskMetadata {
        source_path,
        source_filename: source_filename.clone(),
        source_size,
        total_chunks,
    };

    // 更新全局状态 AppState
    {
        let mut context = state.0.lock().unwrap();

        info!(
            file = %task_metadata.source_filename,
            size = %format_size(task_metadata.source_size),
            total_chunks = task_metadata.total_chunks,
            "任务元数据已创建，正在初始化全局状态"
        );
        context.metadata = Some(task_metadata);
    }

    // 构造成功响应 ---
    let body = format!(
        "文件: {}，大小: {}，预计分块数: {}",
        source_filename,
        format_size(source_size),
        total_chunks
    );

    Ok((StatusCode::OK, body))
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

#[instrument(skip(state), fields(chunk_id = chunk_id))]
pub async fn acknowledge(State(state): State<AppState>, Path(chunk_id): Path<u32>) -> StatusCode {
    info!("接收到分块确认");

    // 1. 锁定全局状态以获取 event_sender
    let context = state.0.lock().unwrap();

    // 2. 检查 event_sender 是否存在
    if let Some(sender) = &context.event_sender {
        // 3. 创建确认事件
        let event = TaskEvent::ChunkAcknowledged { chunk_id };

        // 4. 发送（广播）事件
        // sender.send() 的返回值是 Result<usize, SendError>
        // usize 是收到了这个事件的订阅者数量
        match sender.send(event) {
            Ok(receiver_count) => {
                if receiver_count > 0 {
                    info!(
                        "已将确认事件广播给 {} 个活动的 worker/listener",
                        receiver_count
                    );
                } else {
                    // 这可能发生在对应的 worker 已经超时并退出的情况
                    warn!("确认事件已发送，但没有活动的接收者。可能对应的 worker 已经超时。");
                }
                StatusCode::OK
            }
            Err(_) => {
                // 如果发送失败，通常意味着所有订阅者（包括 SSE 流）都已经消失了
                // 这表明整个 run_task 可能已经结束
                error!("无法广播确认事件，任务可能已经终止。");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    } else {
        // 如果 event_sender 不存在，说明任务从未开始或已经完全清理
        warn!("收到确认请求，但任务当前未在运行状态 (event_sender is None)。");
        // 返回一个客户端错误，因为客户端在一个无效的任务上发送了确认
        StatusCode::BAD_REQUEST
    }
}
