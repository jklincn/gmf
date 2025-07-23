mod handler;
mod state;
mod worker;

use axum::{
    Router,
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use time::macros::format_description;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use tracing_subscriber::{
    EnvFilter, fmt::time::UtcTime, layer::SubscriberExt, util::SubscriberInitExt,
};

fn set_log() {
    let log_file_path = "gmf-remote.log";
    let log_file = std::fs::File::create(log_file_path).expect("无法创建日志文件");

    let filter = EnvFilter::new("info");

    let timer_format = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let timer = UtcTime::new(timer_format);

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(log_file)
                .with_ansi(false)
                .with_timer(timer),
        )
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    set_log();

    let pid = std::process::id();
    println!("{}", pid);

    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_string()])?;
    let tls_config = RustlsConfig::from_pem(
        cert.pem().into_bytes(),
        signing_key.serialize_pem().into_bytes(),
    )
    .await?;
    let app_state = state::AppState::new();

    let app = Router::new()
        .route("/", get(handler::healthy))
        .route("/setup", post(handler::setup))
        .route("/start", get(handler::start))
        .route("/acknowledge/{chunk_id}", post(handler::acknowledge))
        .with_state(app_state)
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http());

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    println!("{}", listener.local_addr()?.port());
    axum_server::from_tcp_rustls(listener, tls_config)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}
