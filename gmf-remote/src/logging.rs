use std::env;
use tracing_appender::non_blocking::WorkerGuard;

pub fn init_logging() -> Result<Option<WorkerGuard>, Box<dyn std::error::Error>> {
    let log_setting = env::var("LOG").unwrap_or_else(|_| "None".to_string());

    if log_setting == "None" {
        Ok(None)
    } else {
        let log_file = ".gmf-remote.log";
        let file = std::fs::File::create(log_file)?;
        let (non_blocking_writer, guard) = tracing_appender::non_blocking(file);

        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(non_blocking_writer)
            .with_ansi(false)
            .finish();

        tracing::subscriber::set_global_default(subscriber)?;

        Ok(Some(guard))
    }
}
