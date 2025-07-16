use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use std::fs;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(value_parser = expand_tilde)]
    path: PathBuf,
}

fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    let expanded = shellexpand::tilde(raw).into_owned();
    if expanded.is_empty() {
        Err("路径不能为空".to_string())
    } else {
        Ok(PathBuf::from(expanded))
    }
}

fn main() -> Result<()> {
    // 1. 解析命令行参数
    let args = Args::parse();
    let filepath = &args.path;
    
    // 2. 获取文件的元数据
    // 我们直接对用户提供的路径（已展开'~'）进行操作
    let metadata = fs::metadata(filepath)
        .map_err(|e| anyhow!("无法获取路径 {:?} 的元数据: {}", filepath, e))?;

    // 3. 检查路径是否指向一个文件
    if !metadata.is_file() {
        return Err(anyhow!("提供的路径 {:?} 不是一个文件", filepath));
    }

    // 4. 获取文件大小 (单位: 字节)
    let file_size = metadata.len();

    // 5. 打印结果
    // 使用 .display() 方法可以更好地打印路径
    println!("文件: {}", filepath.display());
    println!("大小: {} 字节", file_size);

    Ok(())
}