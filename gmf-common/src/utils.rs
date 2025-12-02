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
    gmf_dir.join("config.json")
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

pub fn find_available_filename(path: &PathBuf) -> PathBuf {
    if !path.exists() {
        return path.clone();
    }

    let parent = path.parent().unwrap();
    let file_stem = path.file_stem().unwrap().to_string_lossy();
    let extension = path.extension().map(|e| e.to_string_lossy());

    // filename.ext → filename (1).ext
    for i in 1..10000 {
        let new_name = if let Some(ext) = &extension {
            format!("{} ({}).{}", file_stem, i, ext)
        } else {
            format!("{} ({})", file_stem, i)
        };

        let new_path = parent.join(new_name);
        if !new_path.exists() {
            return new_path;
        }
    }

    // 防御性代码
    path.clone()
}

/// 解析路径字符串，处理首尾引号和波浪号(~)展开
pub fn resolve_path(path_str: &str) -> PathBuf {
    let mut p = path_str.trim();
    // 去除首尾引号
    if (p.starts_with('"') && p.ends_with('"')) || (p.starts_with('\'') && p.ends_with('\'')) {
        p = &p[1..p.len() - 1];
    }

    if p.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            if p == "~" {
                return home;
            }
            // 处理 ~/ 或 ~\
            if p.starts_with("~/") || p.starts_with("~\\") {
                return home.join(&p[2..]);
            }
        }
    }

    PathBuf::from(p)
}

pub trait Xxh3Hashable {
    fn compute_xxh3(&self) -> Result<String>;
}

impl Xxh3Hashable for Path {
    fn compute_xxh3(&self) -> Result<String> {
        let mut input = File::open(self)
            .with_context(|| format!("打开文件 '{}' 失败用于计算 XXH3", self.display()))?;

        let mut hasher = Xxh3::new();

        io::copy(&mut input, &mut hasher)
            .with_context(|| format!("读取文件 '{}' 内容失败用于计算 XXH3", self.display()))?;

        let hash = hasher.finish();
        Ok(format!("{hash:x}"))
    }
}

impl Xxh3Hashable for PathBuf {
    fn compute_xxh3(&self) -> Result<String> {
        self.as_path().compute_xxh3()
    }
}

impl Xxh3Hashable for str {
    fn compute_xxh3(&self) -> Result<String> {
        let mut hasher = Xxh3::new();
        hasher.update(self.as_bytes());
        let hash = hasher.finish();
        Ok(format!("{hash:x}"))
    }
}

/// 计算 XXH3 哈希值
pub fn calc_xxh3<T: Xxh3Hashable + ?Sized>(input: &T) -> Result<String> {
    input.compute_xxh3()
}