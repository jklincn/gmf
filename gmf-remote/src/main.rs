mod handler;
mod r2;
mod state;
mod worker;

use anyhow::anyhow;
use axum::{
    Router,
    routing::{get, post},
};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::crypto::{CryptoProvider, ring};
use time::macros::format_description;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use tracing_subscriber::{fmt::time::UtcTime, layer::SubscriberExt, util::SubscriberInitExt};

fn set_log() {
    let log_file_path = ".gmf-remote.log";
    let log_file = std::fs::File::create(log_file_path).expect("无法创建日志文件");

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let timer_format = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let timer = UtcTime::new(timer_format);

    tracing_subscriber::registry()
        .with(filter)
        .with(
            // 文件日志配置
            tracing_subscriber::fmt::layer()
                .with_writer(log_file)
                .with_ansi(false)
                .with_timer(timer.clone()),
        )
        // .with(
        //     // 控制台日志配置，用于本地debug
        //     tracing_subscriber::fmt::layer()
        //         .with_writer(std::io::stderr)
        //         .with_timer(timer),
        // )
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    set_log();

    CryptoProvider::install_default(ring::default_provider())
        .map_err(|e| anyhow!("Failed to install default rustls crypto provider: {:?}", e))?;

    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_string()])
            .map_err(|e| anyhow!("generate self-signed cert failed: {:?}", e))?;

    let tls_config = RustlsConfig::from_pem(
        cert.pem().into_bytes(),
        signing_key.serialize_pem().into_bytes(),
    )
    .await?;

    let handle = Handle::new();
    let app_state = state::AppState::new(handle.clone());

    let app = Router::new()
        .route("/", get(handler::healthy))
        .route("/setup", post(handler::setup))
        .route("/start", post(handler::start))
        .route("/shutdown", post(handler::shutdown))
        .with_state(app_state)
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http());

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    println!("{}", listener.local_addr()?.port());

    axum_server::from_tcp_rustls(listener, tls_config)
        .handle(handle)
        .serve(app.into_make_service())
        .await?;

    tracing::info!("Server process is about to exit.");

    Ok(())
}
