use std::time::{Duration, SystemTime};

use anyhow::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum LogLevel {
    Error = 0,
    Info = 1,
    Debug = 2,
}

pub struct AllProgressBar {
    pub completed_chunks: u64,
    log_level: LogLevel,

    mp: MultiProgress,
    download: ProgressBar,
}

impl AllProgressBar {
    pub fn new(total_chunks: u64, completed_chunks: u64, log_level: LogLevel) -> Result<Self> {
        let mp: MultiProgress = MultiProgress::new();
        let download = mp.add(ProgressBar::new(total_chunks));

        download.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        download.set_position(completed_chunks);

        Ok(AllProgressBar {
            completed_chunks,
            log_level, // 初始化日志等级
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

    pub fn start_tick(&self) {
        self.download.enable_steady_tick(Duration::from_secs(1));
    }

    /// 在进度条上方输出消息，不会干扰进度条显示
    fn println(&self, msg: &str) {
        self.mp.println(msg).unwrap_or_else(|_| {
            println!("{msg}");
        });
    }

    /// 私有的核心日志记录函数
    fn log(&self, level: LogLevel, msg: &str) {
        if level <= self.log_level {
            let level_str = match level {
                LogLevel::Error => "[ERROR]",
                LogLevel::Info => "[INFO]",
                LogLevel::Debug => "[DEBUG]",
            };

            if let Ok(duration) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                let timestamp = duration.as_secs() + 8 * 3600;
                let hours = (timestamp / 3600) % 24;
                let minutes = (timestamp / 60) % 60;
                let seconds = timestamp % 60;
                self.println(&format!(
                    "[{hours:02}:{minutes:02}:{seconds:02}] {level_str} {msg}"
                ));
            } else {
                self.println(&format!("[--:--:--] {level_str} {msg}"));
            }
        }
    }

    /// 记录一条 ERROR 等级的日志
    #[allow(unused)]
    pub fn log_error(&self, msg: &str) {
        self.log(LogLevel::Error, msg);
    }

    /// 记录一条 INFO 等级的日志
    #[allow(unused)]
    pub fn log_info(&self, msg: &str) {
        self.log(LogLevel::Info, msg);
    }

    /// 记录一条 DEBUG 等级的日志
    #[allow(unused)]
    pub fn log_debug(&self, msg: &str) {
        self.log(LogLevel::Debug, msg);
    }
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
        self.sp.finish_with_message(format!("{msg}"));
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
            // 成功
            spinner.finish(&format!("{}", success_msg));
            Ok(value)
        }
        Err(error) => {
            // 失败
            let error_message = format!("❌ 运行失败: {}", error);
            spinner.finish(&error_message);
            Err(error)
        }
    }
}
