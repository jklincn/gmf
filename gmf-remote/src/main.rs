// src/main.rs

use anyhow::Result;
use axum::{
    Router,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use r2::{ManifestFile, split_and_encrypt};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    let expanded = shellexpand::tilde(raw).into_owned();
    if expanded.is_empty() {
        Err("路径不能为空".to_string())
    } else {
        Ok(PathBuf::from(expanded))
    }
}

async fn split_handler(Json(mut payload): Json<HashMap<String, String>>) -> impl IntoResponse {
    // 1) 校验 filepath
    let filepath = match payload.remove("filepath") {
        Some(fp) => fp,
        None => {
            // 构造统一格式的错误响应
            let mut resp = HashMap::new();
            resp.insert(
                "content".to_string(),
                "missing `filepath` field".to_string(),
            );
            return (StatusCode::BAD_REQUEST, Json(resp));
        }
    };

    // 2) 转成 PathBuf
    let path: PathBuf = filepath.into();

    // 3) 调用业务逻辑
    match split_and_encrypt(&path).await {
        Ok(manifest) => {
            // 把 ManifestFile 序列化成 JSON 字符串
            let json_str = match serde_json::to_string(&manifest) {
                Ok(s) => s,
                Err(e) => {
                    let mut resp = HashMap::new();
                    resp.insert("content".to_string(), format!("序列化失败：{}", e));
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(resp));
                }
            };
            let mut resp = HashMap::new();
            resp.insert("content".to_string(), json_str);
            (StatusCode::OK, Json(resp))
        }
        Err(e) => {
            let mut resp = HashMap::new();
            resp.insert("content".to_string(), e.to_string());
            (StatusCode::INTERNAL_SERVER_ERROR, Json(resp))
        }
    }
}
