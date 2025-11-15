use std::sync::atomic::{AtomicU8, Ordering};
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
    Debug = 3,
}

#[repr(u8)]
enum ProgressBarState {
    NotStarted = 0,
    Running = 1,
    Finished = 2,
}

pub struct AllProgressBar {
    log_level: LogLevel,
    mp: MultiProgress,
    download: OnceCell<ProgressBar>,
    state: AtomicU8,
}

impl AllProgressBar {
    fn new(log_level: LogLevel) -> Self {
        AllProgressBar {
            log_level,
            mp: MultiProgress::new(),
            download: OnceCell::new(),
            state: AtomicU8::new(ProgressBarState::NotStarted as u8),
        }
    }

    fn init_download_bar(&self, total_chunks: u64, completed_chunks: u64) -> Result<()> {
        let download_bar = self.mp.add(ProgressBar::new(total_chunks));

        download_bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green}  [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );
        download_bar.set_position(completed_chunks);

        self.download
            .set(download_bar)
            .map_err(|_| anyhow::anyhow!("下载进度条已经初始化过了"))?;

        Ok(())
    }

    fn get_download_bar(&self) -> &ProgressBar {
        self.download
            .get()
            .expect("下载进度条(download)未初始化，请先调用 init_global_download_bar")
    }

    pub fn update_download(&self) {
        self.get_download_bar().inc(1);
    }

    pub fn finish_download(&self) {
        self.get_download_bar().finish();
        self.state
            .store(ProgressBarState::Finished as u8, Ordering::SeqCst);
    }

    pub fn abandon_download(&self) {
        self.get_download_bar().abandon();
        self.state
            .store(ProgressBarState::Finished as u8, Ordering::SeqCst);
    }

    pub fn start_tick(&self) {
        self.state
            .store(ProgressBarState::Running as u8, Ordering::SeqCst);
        self.get_download_bar()
            .enable_steady_tick(Duration::from_millis(500));
    }

    fn println(&self, msg: &str) {
        let current_state = self.state.load(Ordering::SeqCst);

        match current_state {
            s if s == ProgressBarState::Running as u8 => {
                self.mp.println(msg).unwrap_or_else(|_| {
                    println!("{msg}");
                });
            }
            _ => {
                println!("{msg}");
            }
        }
    }

    fn log(&self, level: LogLevel, msg: &str) {
        if level <= self.log_level {
            if level == LogLevel::Info {
                self.println(msg);
            } else {
                let level_str = match level {
                    LogLevel::Error => "ERROR",
                    LogLevel::Warn => "WARN",
                    LogLevel::Info => "INFO",
                    LogLevel::Debug => "INFO", // 在 UI 界面，INFO 不输出等级，Debug 展示成 INFO
                };
                let timestamp_str = chrono::Local::now().format("%H:%M:%S%.3f");
                self.println(&format!("[{timestamp_str}] [{level_str}] {msg}"));
            }
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
    pub fn log_debug(&self, msg: &str) {
        self.log(LogLevel::Debug, msg);
    }
}

pub fn init_global_logger(verbose: bool) -> Result<()> {
    let log_level = if verbose {
        LogLevel::Debug
    } else {
        LogLevel::Info
    };
    let progress_bar = AllProgressBar::new(log_level);
    G_PROGRESS_BAR
        .set(progress_bar)
        .map_err(|_| anyhow::anyhow!("全局日志/进度条模块已经初始化过了"))?;
    Ok(())
}

pub fn init_global_download_bar(total_chunks: u64, completed_chunks: u64) -> Result<()> {
    global().init_download_bar(total_chunks, completed_chunks)
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
pub fn log_debug(msg: &str) {
    global().log_debug(msg);
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
        .expect("全局日志/进度条(G_PROGRESS_BAR)未初始化，请先调用 init_global_logger")
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
