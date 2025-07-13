use crate::remote::ManifestFile;
use anyhow::{Context, Result, anyhow};
use openssl::{
    hash::MessageDigest,
    pkcs5::pbkdf2_hmac,
    symm::{Cipher, Crypter, Mode},
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

/// 解密所有分块并顺序合并为完整文件
pub fn decrypt_and_merge_local(manifest: &ManifestFile, chunks_dir: &str) -> Result<()> {
    // 0. 输出文件（如已存在会覆盖）
    let mut out = File::create(&manifest.filename)
        .with_context(|| format!("创建输出文件失败: {:?}", &manifest.filename))?;

    let mut total_written = 0u64;

    // 2. 解密每个分块
    for chunk in &manifest.chunks {
        let chunk_path = Path::new(chunks_dir).join(&chunk.filename);
        println!("→ 解密分块 {} ({})", chunk.id, chunk_path.display());

        // 2.1 读完整个加密文件
        let mut enc = Vec::<u8>::new();
        File::open(&chunk_path)
            .with_context(|| format!("打开分块失败 {}", chunk_path.display()))?
            .read_to_end(&mut enc)?;

        // 2.2 传输完整性校验 (加密态 SHA-256)
        let calc_sha = hex::encode(Sha256::digest(&enc));
        if calc_sha != chunk.sha256 {
            return Err(anyhow!(
                "分块 {} SHA-256 不一致 (manifest={}, 计算={})",
                chunk.id,
                chunk.sha256,
                calc_sha
            ));
        }

        // 2.3 解析 OpenSSL salt 头
        if enc.len() < 16 || &enc[..8] != b"Salted__" {
            return Err(anyhow!("分块 {} 缺少 \"Salted__\" 头", chunk.id));
        }
        let salt = &enc[8..16];
        let ciphertext = &enc[16..];

        // 2.4 PBKDF2-SHA256 派生 key & iv
        let passphrase = chunk.passphrase_b64.as_bytes();
        let mut key_iv = [0u8; 48];
        pbkdf2_hmac(
            &passphrase,
            salt,
            manifest.kdf_iter as usize,
            MessageDigest::sha256(),
            &mut key_iv,
        )?;
        let (key, iv) = key_iv.split_at(32);

        // 2.5 解密 (AES-256-CBC + PKCS7 padding)
        let cipher = match manifest.cipher.as_str() {
            "AES-256-CBC" => Cipher::aes_256_cbc(),
            other => return Err(anyhow!("暂不支持算法 {}", other)),
        };

        // -- 使用 streaming API，内存占用适中 --
        let mut crypter = Crypter::new(cipher, Mode::Decrypt, key, Some(iv))?;
        crypter.pad(true);

        let mut plain = vec![0u8; ciphertext.len() + cipher.block_size()];
        let mut count = crypter.update(ciphertext, &mut plain)?;
        count += crypter.finalize(&mut plain[count..])?;
        plain.truncate(count);

        // 2.6 追加写入输出文件
        out.write_all(&plain)?;
        total_written += plain.len() as u64;
        println!("   ✓ OK ({} bytes)", plain.len());
    }

    // 3. 总大小校验
    if total_written != manifest.total_size {
        return Err(anyhow!(
            "解密完成但大小不符: manifest={} 实际={}",
            manifest.total_size,
            total_written
        ));
    }
    println!(
        "🎉 解密合并完成, 输出: {:?} ({} bytes)",
        &manifest.filename, total_written
    );
    Ok(())
}
