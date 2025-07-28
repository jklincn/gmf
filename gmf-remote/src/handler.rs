use crate::state::{AppState, TaskMetadata};
use crate::worker::run_task;
use anyhow::Context;
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
use gmf_common::{
    SetupRequestPayload, SetupResponse, StartRequestPayload, TaskEvent, file_size, format_size,
};
use std::time::Duration;
use std::{convert::Infallible, path::PathBuf};
use tokio::sync::broadcast;
use tracing::{error, info, instrument, warn};

#[instrument]
pub async fn healthy() -> Result<impl IntoResponse, Response> {
    info!("健康检查请求");
    Ok(StatusCode::OK)
}

#[instrument(skip(state, payload))]
pub async fn setup(
    State(state): State<AppState>,
    Json(payload): Json<SetupRequestPayload>,
) -> Result<impl IntoResponse, Response> {
    info!(
        "设置任务: 文件路径 = {}, 分块大小 = {}, 并发数 = {}",
        payload.path,
        format_size(payload.chunk_size),
        payload.concurrency
    );
    if payload.chunk_size == 0 {
        let msg = "chunk_size 不能为 0".to_string();
        return Err((StatusCode::BAD_REQUEST, msg).into_response());
    }

    info!(
        "正在处理文件路径: {}",
        shellexpand::tilde(&payload.path).to_string()
    );

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

    let source_size = file_size(&source_path)
        .with_context(|| format!("获取文件 '{}' 的大小失败", source_path.display()))
        .map_err(|e| {
            let msg = e.to_string();
            (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
        })?;

    let total_chunks = (source_size as f64 / payload.chunk_size as f64).ceil() as u64;

    info!(
        "源文件大小: {}, 分块大小: {}, 总分块数: {}",
        format_size(source_size),
        format_size(payload.chunk_size),
        total_chunks
    );

    // 初始化 TaskMetadata
    let task_metadata = TaskMetadata {
        source_path,
        source_filename: source_filename.clone(),
        source_size,
        chunk_size: payload.chunk_size,
        total_chunks,
        concurrency: payload.concurrency,
    };

    // 更新全局状态
    {
        let mut context = state.0.lock().unwrap();
        info!(
            file = %task_metadata.source_filename,
            size = %format_size(task_metadata.source_size),
            chunk_size = %format_size(task_metadata.chunk_size.try_into().unwrap()),
            total_chunks = task_metadata.total_chunks,
            "任务元数据已创建，正在初始化全局状态"
        );
        context.metadata = Some(task_metadata);
    }

    info!("全局状态已更新，准备返回响应");
    let response_data = SetupResponse {
        filename: source_filename,
        size: source_size,
        total_chunks,
    };

    Ok((StatusCode::OK, Json(response_data)))
}

/// curl -v -N -k -H "Content-Type: application/json" -H "Accept: text/event-stream" -d '{"resume_from_chunk_id": 0}' https://127.0.0.1:39567/start
#[instrument(skip(state, payload), name = "start_task_sse_stream", fields(resume_from = %payload.resume_from_chunk_id))]
pub async fn start(
    State(state): State<AppState>,
    Json(payload): Json<StartRequestPayload>,
) -> Result<impl IntoResponse, Response> {
    let (tx, mut rx) = broadcast::channel::<TaskEvent>(128);
    {
        let mut context = state.0.lock().unwrap();
        if context.event_sender.is_some() {
            warn!("任务已在运行，拒绝了新的 start 请求");
            let msg = "Task is already running.".to_string();
            return Err((StatusCode::CONFLICT, msg).into_response());
        }
        context.event_sender = Some(tx.clone());
    }

    tokio::spawn(run_task(state.clone(), payload.resume_from_chunk_id));

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

    Ok(Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

#[instrument(skip(state))]
pub async fn shutdown(State(state): State<AppState>) -> Result<impl IntoResponse, Response> {
    let context = state.0.lock().unwrap();

    info!("正在关闭服务...");

    context
        .shutdown_handle
        .graceful_shutdown(Some(Duration::from_secs(5)));

    Ok(StatusCode::OK)
}
