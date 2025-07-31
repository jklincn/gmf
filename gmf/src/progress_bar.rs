use std::time::SystemTime;

use anyhow::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// 日志输出等级
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum LogLevel {
    Error = 0,
    Info = 1,
    Debug = 2,
}

pub struct AllProgressBar {
    pub completed_chunks: u64,
    log_level: LogLevel, // 新增：存储当前日志等级

    mp: MultiProgress,
    upload: ProgressBar,
    download: ProgressBar,
    writer: ProgressBar,
}

impl AllProgressBar {
    /// 创建一个新的 AllProgressBar 实例
    pub fn new(total_chunks: u64, completed_chunks: u64, log_level: LogLevel) -> Result<Self> {
        let mp: MultiProgress = MultiProgress::new();
        let upload = mp.add(ProgressBar::new(total_chunks));
        let download = mp.add(ProgressBar::new(total_chunks));
        let writer = mp.add(ProgressBar::new(total_chunks));

        upload.set_style(
            ProgressStyle::default_bar()
                .template("上传进度 [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        download.set_style(
            ProgressStyle::default_bar()
                .template("下载进度 [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        writer.set_style(
            ProgressStyle::default_bar()
                .template("写入进度 [{bar:40.cyan/blue}] {percent}% ({pos}/{len}) | ETD: {elapsed_precise} | ETA: {eta_precise}")?
                .progress_chars("#>-"),
        );

        upload.set_position(completed_chunks);
        download.set_position(completed_chunks);
        writer.set_position(completed_chunks);

        Ok(AllProgressBar {
            completed_chunks,
            log_level, // 初始化日志等级
            mp,
            upload,
            download,
            writer,
        })
    }

    pub fn update_upload(&self) {
        self.upload.inc(1);
    }
    pub fn update_download(&self) {
        self.download.inc(1);
    }
    pub fn update_writer(&self) {
        self.writer.inc(1);
    }

    /// 设置所有进度条为完成状态
    pub fn finish_all(&self) {
        self.upload.finish();
        self.download.finish();
        self.writer.finish();
    }

    /// 在进度条上方输出消息，不会干扰进度条显示
    fn println(&self, msg: &str) {
        self.mp.println(msg).unwrap_or_else(|_| {
            println!("{}", msg);
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
                // 东八区时间
                let timestamp = duration.as_secs() + 8 * 3600;
                let hours = (timestamp / 3600) % 24;
                let minutes = (timestamp / 60) % 60;
                let seconds = timestamp % 60;
                self.println(&format!(
                    "[{:02}:{:02}:{:02}] {} {}",
                    hours, minutes, seconds, level_str, msg
                ));
            } else {
                self.println(&format!("[--:--:--] {} {}", level_str, msg));
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
