use anyhow::{Context, Result};
use std::{fs, path::PathBuf};
use std::{fs::File, hash::Hasher, io, path::Path};
use xxhash_rust::xxh3::Xxh3;

pub fn app_dir() -> PathBuf {
    let base = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .expect("Cannot determine config directory");

    let gmf_dir = base.join("gmf");
    fs::create_dir_all(&gmf_dir).expect("Failed to create config directory");
    gmf_dir
}

pub fn config_path() -> PathBuf {
    let gmf_dir = app_dir();
    gmf_dir.join("config.toml")
}

/// 格式化字节大小为易读的字符串
pub fn format_size(size: u64) -> String {
    if size == 0 {
        return "0.00 B".to_string();
    }

    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut size = size as f64;
    let mut unit = 0;

    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }

    format!("{:.2} {}", size, UNITS[unit])
}

/// 计算指定文件的 XXH3 哈希值
pub fn calc_xxh3(path: &Path) -> Result<String> {
    let mut input = File::open(path)
        .with_context(|| format!("打开文件 '{}' 失败用于计算 XXH3", path.display()))?;

    let mut hasher = Xxh3::new();

    io::copy(&mut input, &mut hasher)
        .with_context(|| format!("读取文件 '{}' 内容失败用于计算 XXH3", path.display()))?;

    let hash = hasher.finish();

    Ok(format!("{hash:x}"))
}
