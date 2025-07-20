use anyhow::Result;
use async_stream::stream;
use axum::{
    Router,
    extract::{Json, Query},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use futures_util::stream::{self, Stream};
use r2::split_and_encrypt;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::{convert::Infallible, time::Duration};
use tokio::net::TcpListener;
use tokio_stream::StreamExt as _;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use tracing::{error, info, instrument};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- 日志系统初始化 ---
    // 设置日志文件滚动：每天创建一个新文件，存放在 "logs" 目录下
    let file_appender = tracing_appender::rolling::daily("logs", "server.log");
    let (non_blocking_writer, _guard) = tracing_appender::non_blocking(file_appender);

    // 创建一个将日志格式化为 JSON 并写入文件的层
    let file_layer = tracing_subscriber::fmt::layer()
        .json() // 以 JSON 格式输出
        .with_writer(non_blocking_writer);

    // 创建一个将日志格式化后输出到控制台的层
    let console_layer = tracing_subscriber::fmt::layer().pretty();

    // 从环境变量 RUST_LOG 中读取日志级别，如果没有设置，则默认为 "info"
    let filter = EnvFilter::new("info");

    // 组合各个层并初始化日志系统
    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();
    // --- 日志系统初始化完成 ---

    let pid = std::process::id();
    println!("{}", pid);

    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_string()])?;
    let tls_config = RustlsConfig::from_pem(
        cert.pem().into_bytes(),
        signing_key.serialize_pem().into_bytes(),
    )
    .await?;

    // 添加 TraceLayer 用于自动记录请求信息，CatchPanicLayer 用于捕获 panic
    let app = Router::new()
        .route("/", get(healthy_handler))
        .route("/split", post(split_handler))
        .route("/split_sse", get(sse_handler))
        .layer(CatchPanicLayer::new()) // 捕获 panic, 防止服务器崩溃
        .layer(TraceLayer::new_for_http()); // 自动记录 HTTP 请求

    let ipv6_listener = TcpListener::bind((Ipv6Addr::UNSPECIFIED, 0)).await?;
    let ipv6_addr = ipv6_listener.local_addr()?;
    drop(ipv6_listener);
    println!("{}", ipv6_addr.port());

    axum_server::bind_rustls(ipv6_addr, tls_config)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

async fn healthy_handler() -> StatusCode {
    info!("健康检查请求");
    StatusCode::OK
}

#[derive(Deserialize)]
struct Params {
    path: String,
}
async fn sse_handler(
    Query(params): Query<Params>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // 从 Query 中取出 path
    let path: PathBuf = params.path.into();
    // 构造一个只发两次事件的 Stream：先发一个 status，再执行任务，最后发一个 done
    let event_stream = stream! {
        // （1）可选：先告诉客户端“processing”
        yield Ok::<_, Infallible>(Event::default()
            .event("status")
            .data("processing"));
        sleep(Duration::from_secs(10)).await;
        // （2）调用你的耗时处理函数
        let result = split_and_encrypt(path).await;
        let data = match result {
            Ok(manifest) => {
                // 如果 ManifestFile 可序列化，直接转成 JSON 字符串
                serde_json::to_string(&manifest)
                    .unwrap_or_else(|e| format!("序列化失败: {}", e))
            }
            Err(err) => {
                // 错误时也返回一个描述
                format!("处理出错: {}", err)
            }
        };
        // （3）处理完成后，推送“done”事件并附上结果
        yield Ok(Event::default()
            .event("done")
            .data(data));

    };

    // 可选心跳，防止中间网络设备断开空闲连接
    Sse::new(event_stream).keep_alive(KeepAlive::default())
}

// 使用 `instrument` 宏可以自动为这个函数创建一个日志 span
#[instrument(skip(payload), fields(filepath))]
async fn split_handler(Json(mut payload): Json<HashMap<String, String>>) -> impl IntoResponse {
    let filepath = match payload.remove("filepath") {
        Some(fp) => {
            // 将文件路径记录到 span 中
            tracing::Span::current().record("filepath", &fp);
            fp
        }
        None => {
            error!("请求中缺少 'filepath' 字段");
            let mut resp = HashMap::new();
            resp.insert(
                "content".to_string(),
                "missing `filepath` field".to_string(),
            );
            return (StatusCode::BAD_REQUEST, Json(resp));
        }
    };

    info!("接收到文件分割加密请求");

    let path: PathBuf = filepath.into();
    info!(path = ?&path, "准备调用 split_and_encrypt");

    let result = split_and_encrypt(&path).await;

    info!("split_and_encrypt 调用完成");

    match result {
        Ok(manifest) => {
            let json_str = match serde_json::to_string(&manifest) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "序列化 ManifestFile 失败");
                    let mut resp = HashMap::new();
                    resp.insert("content".to_string(), format!("序列化失败：{}", e));
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(resp));
                }
            };
            let mut resp = HashMap::new();
            resp.insert("content".to_string(), json_str);
            info!("文件处理成功，返回 manifest");
            (StatusCode::OK, Json(resp))
        }
        Err(e) => {
            error!(error = %e, "split_and_encrypt 执行失败");
            let mut resp = HashMap::new();
            resp.insert("content".to_string(), e.to_string());
            (StatusCode::INTERNAL_SERVER_ERROR, Json(resp))
        }
    }
}
