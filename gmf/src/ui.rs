use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::OnceCell;

static G_PROGRESS_BAR: OnceCell<AllProgressBar> = OnceCell::const_new();

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
}

pub struct AllProgressBar {
    log_level: LogLevel,
    mp: MultiProgress,
    download: ProgressBar,
}

impl AllProgressBar {
    fn new(total_chunks: u64, completed_chunks: u64, log_level: LogLevel) -> Result<Self> {
        let mp: MultiProgress = MultiProgress::new();
        let download = mp.add(ProgressBar::new(total_chunks));

        download.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );
        download.set_position(completed_chunks);
        Ok(AllProgressBar {
            log_level,
            mp,
            download,
        })
    }

    pub fn update_download(&self) {
        self.download.inc(1);
    }

    pub fn finish_download(&self) {
        self.download.finish();
    }

    pub fn abandon_download(&self) {
        self.download.abandon();
    }

    pub fn start_tick(&self) {
        self.download.enable_steady_tick(Duration::from_millis(500));
    }

    fn println(&self, msg: &str) {
        self.mp.println(msg).unwrap_or_else(|_| {
            println!("{msg}");
        });
    }

    fn log(&self, level: LogLevel, msg: &str) {
        if level <= self.log_level {
            let level_str = match level {
                LogLevel::Error => "[ERROR]",
                LogLevel::Warn => "[WARN]",
                LogLevel::Info => "[INFO]",
            };
            let timestamp_str = chrono::Local::now().format("%H:%M:%S");
            self.println(&format!("[{timestamp_str}] {level_str} {msg}"));
        }
    }

    pub fn log_error(&self, msg: &str) {
        self.log(LogLevel::Error, msg);
    }

    pub fn log_warn(&self, msg: &str) {
        self.log(LogLevel::Warn, msg);
    }

    pub fn log_info(&self, msg: &str) {
        self.log(LogLevel::Info, msg);
    }
}

// 初始化全局进度条
pub fn init_global_progress_bar(
    total_chunks: u64,
    completed_chunks: u64,
    log_level: LogLevel,
) -> Result<()> {
    let progress_bar = AllProgressBar::new(total_chunks, completed_chunks, log_level)?;
    G_PROGRESS_BAR
        .set(progress_bar)
        .map_err(|_| anyhow::anyhow!("全局进度条已经初始化过了"))?;
    Ok(())
}

pub fn start_tick() {
    global().start_tick();
}

pub fn update_download() {
    global().update_download();
}

pub fn finish_download() {
    global().finish_download();
}

pub fn abandon_download() {
    global().abandon_download();
}

#[allow(unused)]
pub fn log_info(msg: &str) {
    global().log_info(msg);
}

#[allow(unused)]
pub fn log_warn(msg: &str) {
    global().log_warn(msg);
}

#[allow(unused)]
pub fn log_error(msg: &str) {
    global().log_error(msg);
}

fn global() -> &'static AllProgressBar {
    G_PROGRESS_BAR
        .get()
        .expect("全局进度条(G_PROGRESS_BAR)未初始化，请先调用 init_global_progress_bar")
}

pub struct Spinner {
    sp: ProgressBar,
}

impl Spinner {
    pub fn new(msg: &str) -> Self {
        let spinner_style = ProgressStyle::default_spinner()
            .template("{spinner:.green}  {msg}")
            .expect("设置样式失败");

        let sp = ProgressBar::new_spinner();
        sp.set_style(spinner_style);
        sp.set_message(msg.to_string());
        sp.enable_steady_tick(Duration::from_millis(100));

        Self { sp }
    }

    pub fn finish(self, msg: &str) {
        let finish_template = ProgressStyle::with_template("{msg}").expect("创建完成样式失败");
        self.sp.set_style(finish_template);
        self.sp.finish_with_message(msg.to_string());
    }

    pub fn abandon(self) {
        self.sp.abandon();
    }
}

pub async fn run_with_spinner<F, T, E>(
    loading_msg: &str,
    success_msg: &str,
    task: F,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let spinner = Spinner::new(loading_msg);

    match task.await {
        Ok(value) => {
            spinner.finish(success_msg);
            Ok(value)
        }
        Err(error) => {
            let error_message = format!("❌ 运行失败: {error}");
            spinner.finish(&error_message);
            Err(error)
        }
    }
}
