// src/main.rs

use anyhow::Result;
use axum::{
    Router,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pid = std::process::id();
    println!("{}", pid);
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_string()])?;
    let tls_config = RustlsConfig::from_pem(
        cert.pem().into_bytes(),
        signing_key.serialize_pem().into_bytes(),
    )
    .await
    .unwrap();

    let app = Router::new()
        .route("/", get(healthy_handler))
        .route("/split", post(split_handler));
    let ipv6_listener = TcpListener::bind((Ipv6Addr::UNSPECIFIED, 0)).await?;
    let ipv6_addr = ipv6_listener.local_addr()?;
    drop(ipv6_listener);
    println!("{}", ipv6_addr.port());

    axum_server::bind_rustls(ipv6_addr, tls_config)
        .serve(app.into_make_service())
        .await
        .unwrap();

    Ok(())
}

// --- 处理器函数 (保持不变) ---

async fn healthy_handler() -> StatusCode {
    StatusCode::OK
}

#[derive(Deserialize)]
struct SplitPayload {
    path: String,
}

#[derive(Serialize)]
struct SplitResponse {
    status: String,
    received_path: String,
    message: String,
}

fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    let expanded = shellexpand::tilde(raw).into_owned();
    if expanded.is_empty() {
        Err("路径不能为空".to_string())
    } else {
        Ok(PathBuf::from(expanded))
    }
}

async fn split_handler(Json(payload): Json<SplitPayload>) -> impl IntoResponse {
    println!("接收到 /split 请求，路径为: {}", payload.path);

    match expand_tilde(&payload.path) {
        Ok(expanded_path) => {
            let response = SplitResponse {
                status: "success".to_string(),
                received_path: expanded_path.to_string_lossy().to_string(),
                message: "路径已接收并成功展开。".to_string(),
            };
            (StatusCode::OK, Json(response))
        }
        Err(e) => {
            let response = SplitResponse {
                status: "error".to_string(),
                received_path: payload.path,
                message: e,
            };
            (StatusCode::BAD_REQUEST, Json(response))
        }
    }
}
