use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::{env, fs, path::PathBuf};
use tar::Builder;

// 用于在函数间传递生成的资源信息
struct AssetInfo {
    // 将在生成的 Rust 代码中使用的常量名前缀，例如 "REMOTE_ELF"
    const_prefix: String,
    // 生成的 .tar.gz 文件在 OUT_DIR 中的路径
    targz_path: PathBuf,
    // 原始文件的 SHA256 哈希值
    sha256_hex: String,
}

/// 处理本地构建的 gmf-remote ELF 文件。
/// 它会找到 ELF，将其打包成 tar，然后用 Gzip 压缩。
fn generate_gmf_remote_asset(out_dir: &PathBuf, profile: &str) -> AssetInfo {
    println!("cargo:rerun-if-changed=crates/gmf-remote"); // 依赖 gmf-remote crate

    let workspace_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_owned();

    let remote_elf_path = workspace_root
        .join("target")
        .join("x86_64-unknown-linux-musl")
        .join(profile)
        .join("gmf-remote");

    println!("cargo:rerun-if-changed={}", remote_elf_path.display());

    if !remote_elf_path.exists() {
        panic!(
            "gmf-remote ELF not found at: {}. Please build the 'gmf-remote' crate first with --profile {}",
            remote_elf_path.display(),
            profile
        );
    }

    // 1. 读取原始 ELF 文件
    let original_bytes = fs::read(&remote_elf_path).expect("Failed to read gmf-remote ELF");

    // 2. 计算原始文件的 SHA256
    let sha256 = Sha256::digest(&original_bytes);
    let sha256_hex = format!("{:x}", sha256);

    // 3. 将 ELF 打包进 tar 归档
    let mut tar_builder = Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(original_bytes.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar_builder
        .append_data(&mut header, "gmf-remote", original_bytes.as_slice())
        .expect("Failed to append gmf-remote to tar archive");
    let tar_bytes = tar_builder
        .into_inner()
        .expect("Failed to finalize tar archive");

    // 4. Gzip 压缩 tar 归档
    let compression_level = if profile == "release" {
        println!("cargo:warning=gmf-remote: Using max compression for release build.");
        Compression::best()
    } else {
        Compression::none()
    };
    let mut enc = GzEncoder::new(Vec::new(), compression_level);
    enc.write_all(&tar_bytes).unwrap();
    let targz_bytes = enc.finish().unwrap();

    // 5. 将压缩文件写入 OUT_DIR
    let targz_path = out_dir.join("gmf-remote.tar.gz");
    fs::write(&targz_path, &targz_bytes).expect("Failed to write gmf-remote.tar.gz");

    AssetInfo {
        const_prefix: "GMF_REMOTE".to_string(),
        targz_path,
        sha256_hex,
    }
}

/// 从 URL 下载 s5cmd 工具。
fn generate_s5cmd_asset(out_dir: &PathBuf) -> AssetInfo {
    const S5CMD_URL: &str =
        "https://github.com/peak/s5cmd/releases/download/v2.3.0/s5cmd_2.3.0_Linux-64bit.tar.gz";

    println!("cargo:rerun-if-changed=build.rs");

    // 1. 基于 URL 生成一个唯一的缓存文件名
    let url_hash = Sha256::digest(S5CMD_URL.as_bytes());
    let url_hash_prefix = &format!("{:x}", url_hash)[..16]; // 取前16个字符作为指纹
    let cache_filename = format!("s5cmd_{}.tar.gz", url_hash_prefix);
    let targz_path = out_dir.join(&cache_filename);

    // 2. 如果缓存文件不存在，则下载
    if !targz_path.exists() {
        println!(
            "cargo:warning=s5cmd cache not found. Downloading from {}",
            S5CMD_URL
        );

        // 清理可能存在的旧版本缓存文件
        for entry in fs::read_dir(out_dir).unwrap() {
            let entry = entry.unwrap();
            if let Some(filename) = entry.file_name().to_str() {
                if filename.starts_with("s5cmd_") && filename.ends_with(".tar.gz") {
                    println!("cargo:warning=Removing old s5cmd cache: {}", filename);
                    fs::remove_file(entry.path()).unwrap();
                }
            }
        }

        // 下载新文件
        let response = reqwest::blocking::get(S5CMD_URL).expect("Failed to download s5cmd");
        if !response.status().is_success() {
            panic!(
                "Failed to download s5cmd: HTTP Status {}",
                response.status()
            );
        }
        let targz_bytes = response
            .bytes()
            .expect("Failed to read s5cmd response bytes");

        // 写入文件到缓存位置
        fs::write(&targz_path, &targz_bytes).expect("Failed to write downloaded s5cmd");
        println!(
            "cargo:warning=s5cmd downloaded and cached as {}",
            cache_filename
        );
    } else {
        println!(
            "cargo:warning=Using cached s5cmd file: {}",
            targz_path.display()
        );
    }

    // 3. 无论来源如何，都读取文件内容并计算哈希
    let final_bytes = fs::read(&targz_path).expect("Failed to read final s5cmd file");
    let sha256_hex = format!("{:x}", Sha256::digest(&final_bytes));

    // 4. 返回资源信息
    AssetInfo {
        const_prefix: "S5CMD".to_string(),
        targz_path,
        sha256_hex,
    }
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let profile = env::var("PROFILE").expect("PROFILE env var not set by Cargo");

    let assets = vec![
        generate_gmf_remote_asset(&out_dir, &profile),
        // generate_s5cmd_asset(&out_dir),
    ];

    let mut final_content = String::new();
    final_content.push_str("// This file is automatically generated by build.rs. Do not edit.\n\n");

    for asset in assets {
        let asset_content = format!(
            r##"pub const {prefix}_TAR_GZ: &[u8] = include_bytes!(r#"{path}"#);
pub const {prefix}_SHA256: &str = "{sha256}";
"##,
            prefix = asset.const_prefix,
            path = asset.targz_path.display(),
            sha256 = asset.sha256_hex
        );

        final_content.push_str(&asset_content);
        final_content.push('\n');
    }

    let embed_rs_path = out_dir.join("embedded_assets.rs");
    fs::write(&embed_rs_path, final_content).expect("Failed to write embedded_assets.rs");
}
