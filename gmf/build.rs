use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::{env, fs, path::PathBuf};

fn main() {
    // workspace 根目录
    let workspace_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_owned();

    let remote_elf = workspace_root
        .join("target")
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("gmf-remote");

    // 读取 ELF
    let bytes = fs::read(&remote_elf).expect("read ELF failed");

    // 压缩 ELF
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&bytes).unwrap();
    let gz_bytes = enc.finish().unwrap();

    // 计算 SHA-256
    let sha256 = Sha256::digest(&bytes);
    let sha256_hex = format!("{:x}", sha256);

    // 写到 gmf 的 OUT_DIR
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let gz_path = out_dir.join("gmf-remote.gz");
    fs::write(&gz_path, &gz_bytes).expect("写入压缩文件失败");

    // 计算 SHA-256（原始 ELF 的）
    let sha256 = Sha256::digest(&bytes);
    let sha256_hex = format!("{:x}", sha256);

    // 生成 Rust 源文件：OUT_DIR/gmf-remote.rs
    let embed_rs = out_dir.join("gmf-remote.rs");
    let content = format!(
        r#"pub const REMOTE_ELF: &[u8] = include_bytes!("{gz_path}");
pub const REMOTE_ELF_SHA256: &str = "{sha256}";
"#,
        gz_path = gz_path.display(),
        sha256 = sha256_hex
    );

    fs::write(&embed_rs, content).expect("写入 gmf-remote.rs 失败");
}
