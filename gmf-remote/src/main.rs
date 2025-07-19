use anyhow::Result;
use axum::{
    extract::Json,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use r2::split_and_encrypt;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use std::collections::HashMap;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use tokio::net::TcpListener;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use tracing::{error, info, instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

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
            resp.insert("content".to_string(), "missing `filepath` field".to_string());
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
