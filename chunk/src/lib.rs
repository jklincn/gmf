use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

const NONCE_SIZE: usize = 12;
const CHUNK_SIZE: usize = 10 * 1024 * 1024; // 10MB

/// 分块信息结构体
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChunkInfo {
    pub id: u32,                // 分块序号（从 1 开始）
    pub filename: String,       // 远程最终文件名
    pub passphrase_b64: String, // Base64-encoded 随机口令
    pub sha256: String,         // 加密后文件哈希
    pub size: u64,              // 加密后大小（字节）
}

/// 整个文件的清单
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ManifestFile {
    pub filename: OsString,
    pub total_size: u64,
    pub chunk_size: u64,
    pub total_chunks: u32,
    pub key: [u8; 32],
    pub chunks: Vec<ChunkInfo>,
}

fn generate_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    key
}

// === 加密文件 ===
fn encrypt_file(key_bytes: &[u8; 32], input_path: &str, output_path: &str) -> anyhow::Result<()> {
    let plaintext = fs::read(input_path)?;

    // 生成随机 nonce（每个文件都唯一）
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|e| anyhow::anyhow!("加密失败: {:?}", e))?;

    // 输出格式：nonce || ciphertext
    let mut result = Vec::from(&nonce_bytes[..]);
    result.extend_from_slice(&ciphertext);

    fs::write(output_path, result)?;
    Ok(())
}

// === 解密文件 ===
fn decrypt_file(key_bytes: &[u8; 32], input_path: &str, output_path: &str) -> anyhow::Result<()> {
    let encrypted = fs::read(input_path)?;

    if encrypted.len() < NONCE_SIZE {
        anyhow::bail!("输入文件太短，无法包含有效 nonce");
    }

    let (nonce_bytes, ciphertext) = encrypted.split_at(NONCE_SIZE);

    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("解密失败: {:?}", e))?;
    fs::write(output_path, plaintext)?;
    Ok(())
}

fn split_file(input_path: &str) -> io::Result<()> {
    let input_file = File::open(input_path)?;
    let mut reader = BufReader::new(input_file);

    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut part_index = 0;

    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }

        let part_name = format!("{}.part{}", input_path, part_index);
        let mut part_file = File::create(&part_name)?;
        part_file.write_all(&buffer[..n])?;

        println!("写入分块：{}", part_name);
        part_index += 1;
    }

    Ok(())
}

/// 合并多个 part 文件成一个
fn merge_files(base_path: &str, output_path: &str) -> io::Result<()> {
    let mut output_file = File::create(output_path)?;
    let mut writer = BufWriter::new(&mut output_file);

    let mut index = 0;
    loop {
        let part_name = format!("{}.part{}", base_path, index);
        let part_path = Path::new(&part_name);
        if !part_path.exists() {
            break;
        }

        let mut part_file = File::open(&part_path)?;
        io::copy(&mut part_file, &mut writer)?;
        println!("合并分块：{}", part_name);

        index += 1;
    }

    writer.flush()?;
    println!("合并完成，输出文件：{}", output_path);
    Ok(())
}
