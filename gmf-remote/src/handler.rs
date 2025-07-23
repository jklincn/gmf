use crate::state::AppState;
use crate::worker::run_task;
use anyhow::Context;
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
use r2::TaskEvent;
use serde::Deserialize;
use std::{convert::Infallible, fs::File};
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
    let mut context = state.0.lock().unwrap();
    context.path = Some(payload.path.clone().into());

    let path = context.path.as_deref().expect("path not set");
    let input_file = File::open(path).map_err(|e| {
        let msg = format!("无法打开文件 `{}`: {}", path.display(), e);
        (StatusCode::BAD_REQUEST, msg).into_response()
    })?;

    let metadata = input_file.metadata().map_err(|e| {
        let msg = format!("无法获取文件 `{}` 的元数据: {}", path.display(), e);
        (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
    })?;
    let file_size = metadata.len();
    let human_size = format_size(file_size);

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>");
    info!("已设置文件: {}，总大小: {}", file_name, human_size);

    let body = format!("文件: {}，大小: {}", file_name, human_size);
    Ok((StatusCode::OK, body))
}

#[instrument(skip_all, name = "start_task_sse_stream")]
pub async fn start(State(state): State<AppState>) -> impl IntoResponse {
    let task_path;
    let (tx, mut rx) = broadcast::channel::<TaskEvent>(128);

    {
        let mut context = state.0.lock().unwrap();
        task_path = context.path.clone().unwrap();
        context.event_sender = Some(tx.clone());
    }

    tokio::spawn(run_task(state.clone(), task_path));

    let event_stream = stream! {
        loop {
            match rx.recv().await {
                Ok(task_event) => {
                    let is_final_event = matches!(task_event, TaskEvent::TaskCompleted | TaskEvent::Error {..});
                    let json_data = serde_json::to_string(&task_event).unwrap();
                    yield Ok::<Event, Infallible>(Event::default().data(json_data));

                    if is_final_event {
                        info!("检测到最终事件，关闭 SSE 流");
                        break;
                    }
                },
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(count = n, "SSE 客户端接收滞后，丢失了 {} 个事件", n);
                },
                Err(broadcast::error::RecvError::Closed) => {
                    info!("广播通道已关闭，SSE 流正常结束");
                    break;
                }
            };
        }
    };

    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[instrument(skip(state), fields(chunk_id = chunk_id))]
pub async fn acknowledge(State(state): State<AppState>, Path(chunk_id): Path<u32>) -> StatusCode {
    info!("接收到分块确认");

    let context = state.0.lock().unwrap();

    if let Some(sender) = &context.event_sender {
        info!("转发确认事件到 worker");
        if sender
            .send(TaskEvent::ChunkAcknowledged { chunk_id })
            .is_err()
        {
            // 这个警告很重要，它意味着 worker 任务可能已经因为某种原因提前结束了
            warn!("事件发送失败，可能没有活动的接收者 (worker 已结束?)");
            // 即使发送失败，从客户端的角度看，这个 acknowledge 请求可能仍然是“可接受的”
            // 所以我们仍然可以返回 OK，但日志是关键
        }
        StatusCode::OK
    } else {
        // 这是一个异常状态：chunks 存在，但任务的 event_sender 却没了
        error!("找到了 chunk，但 event_sender 不存在。这是一个逻辑错误。");
        StatusCode::INTERNAL_SERVER_ERROR
    }
    // --- 修改结束 ---
}
