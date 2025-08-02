use tracing_appender::non_blocking::WorkerGuard;

pub fn init_logging() -> Result<WorkerGuard, Box<dyn std::error::Error>> {
    // 定义日志文件的路径
    let log_file = ".gmf-remote.log";

    // 使用 File::create 会在每次运行时创建新文件或覆盖旧文件
    let file = std::fs::File::create(log_file)?;

    // 配置 tracing-appender，实现非阻塞写入
    let (non_blocking_writer, guard) = tracing_appender::non_blocking(file); // 注意，变量名移除了下划线

    // 构建 subscriber
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(non_blocking_writer)
        .with_ansi(false)
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    // 返回 guard，将其生命周期交给调用者管理
    Ok(guard)
}
