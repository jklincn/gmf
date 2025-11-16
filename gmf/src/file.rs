use crate::ui;
use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose};
use gmf_common::{
    chunk::ChunkBitmap,
    consts::NONCE_SIZE,
    r2,
    utils::{app_dir, find_available_filename},
};
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::Arc;
use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Metadata {
    name: String,
    size: u64,
    xxh3: String,
    local_path: PathBuf,
    remote_path: PathBuf,
    chunk_size: u64,
    total_chunks: u64,
    chunk_bitmap: ChunkBitmap,
}

impl Metadata {
    fn new(
        name: &str,
        size: u64,
        xxh3: &str,
        local_path: PathBuf,
        remote_path: PathBuf,
        chunk_size: u64,
        total_chunks: u64,
    ) -> Result<Self> {
        let metadata_path = app_dir().join(format!("{}.json", xxh3));
        let metadata = Metadata {
            name: name.to_string(),
            size,
            xxh3: xxh3.to_string(),
            local_path,
            remote_path,
            chunk_size,
            total_chunks,
            chunk_bitmap: ChunkBitmap::new(total_chunks),
        };
        metadata.save()?;
        ui::log_debug(&format!("创建新的元数据文件: {}", metadata_path.display()));
        Ok(metadata)
    }

    fn load(
        name: &str,
        size: u64,
        xxh3: &str,
        local_path: PathBuf,
        remote_path: PathBuf,
        chunk_size: u64,
        total_chunks: u64,
    ) -> Result<Self> {
        let metadata_path = app_dir().join(format!("{}.json", xxh3));
        let metadata_content = fs::read_to_string(&metadata_path)
            .with_context(|| format!("读取元数据文件 '{}' 失败", metadata_path.display()))?;
        let metadata: Metadata = serde_json::from_str(&metadata_content)
            .with_context(|| format!("解析元数据文件 '{}' 失败", metadata_path.display()))?;
        // 验证元数据是否匹配
        if metadata.name != name
            || metadata.size != size
            || metadata.xxh3 != xxh3
            || metadata.local_path != local_path
            || metadata.remote_path != remote_path
            || metadata.chunk_size != chunk_size
            || metadata.total_chunks != total_chunks
        {
            bail!(
                "元数据文件 '{}' 内容与当前下载任务不匹配，可能是旧的或损坏的文件",
                metadata_path.display()
            );
        }
        ui::log_info("继续未完成的下载任务");
        Ok(metadata)
    }

    fn path(&self) -> PathBuf {
        app_dir().join(format!("{}.json", self.xxh3))
    }

    fn save(&self) -> Result<()> {
        let metadata_path = self.path();
        let metadata_content =
            serde_json::to_string_pretty(&self).with_context(|| "序列化元数据失败".to_string())?;
        fs::write(&metadata_path, metadata_content)
            .with_context(|| format!("写入元数据文件 '{}' 失败", metadata_path.display()))?;
        Ok(())
    }

    fn total_chunks(&self) -> u64 {
        self.total_chunks
    }

    fn completed_chunks(&self) -> u64 {
        self.chunk_bitmap.completed_count()
    }

    fn is_completed(&self, chunk_id: u64) -> bool {
        self.chunk_bitmap.is_completed(chunk_id)
    }

    fn remove(&self) -> Result<()> {
        let metadata_path = self.path();
        if metadata_path.exists() {
            fs::remove_file(&metadata_path)
                .with_context(|| format!("删除元数据文件 '{}' 失败", metadata_path.display()))?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ChunkResult {
    Success(u64),
    Timeout(u64),
    Failure(u64, anyhow::Error),
}

pub struct DownloadSession {
    file: Arc<Mutex<File>>,
    metadata: Arc<Mutex<Metadata>>,
}

impl DownloadSession {
    pub fn new(
        file_name: &str,
        file_size: u64,
        xxh3: &str,
        remote_path: &str,
        chunk_size: u64,
        total_chunks: u64,
    ) -> Result<(Self, u64)> {
        let cwd: PathBuf = env::current_dir().unwrap();
        let local_path = cwd.join(file_name);
        let temp_path = cwd.join(format!("{}.gmf", xxh3));
        let remote_path = PathBuf::from(remote_path);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&temp_path)
            .with_context(|| format!("打开本地临时文件 '{}' 失败", temp_path.display()))?;
        file.set_len(file_size)?;

        let metadata_path = app_dir().join(format!("{}.json", xxh3));
        let metadata = if metadata_path.exists() {
            Metadata::load(
                file_name,
                file_size,
                xxh3,
                local_path.clone(),
                remote_path.clone(),
                chunk_size,
                total_chunks,
            )?
        } else {
            Metadata::new(
                file_name,
                file_size,
                xxh3,
                local_path.clone(),
                remote_path.clone(),
                chunk_size,
                total_chunks,
            )?
        };

        let completed_chunks = metadata.completed_chunks();
        let session = Self {
            file: Arc::new(Mutex::new(file)),
            metadata: Arc::new(Mutex::new(metadata)),
        };

        Ok((session, completed_chunks))
    }
    pub async fn total_chunks(&self) -> u64 {
        self.metadata.lock().await.total_chunks()
    }
    pub async fn completed_chunks(&self) -> u64 {
        self.metadata.lock().await.completed_chunks()
    }
    pub async fn is_completed(&self, chunk_id: u64) -> bool {
        self.metadata.lock().await.is_completed(chunk_id)
    }
    /// 负责下载分块与解密，写入交由写入器操作
    pub async fn handle_chunk(&self, chunk_id: u64, passphrase_b64: String) -> ChunkResult {
        ui::log_debug(&format!("分块 #{chunk_id} 开始处理"));

        let result: anyhow::Result<()> = async {
            let start_time = std::time::Instant::now();

            // 下载分块
            let content = match r2::get_object(&chunk_id.to_string()).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    let error_msg = format!("{e:?}");
                    ui::log_debug(&format!("分块 #{chunk_id} 下载失败: {error_msg}"));
                    return Err(e.context(format!("分块 #{chunk_id} 下载失败")));
                }
            };

            ui::log_debug(&format!(
                "分块 #{chunk_id} 下载完成，耗时: {:.2?}",
                start_time.elapsed()
            ));

            // 解密分块
            let plain_data = tokio::task::spawn_blocking(move || {
                let key_bytes = general_purpose::STANDARD
                    .decode(passphrase_b64)
                    .context("Base64 解码密钥失败")?;

                let (nonce_bytes, ciphertext) = content.split_at(NONCE_SIZE);
                let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
                let cipher = Aes256Gcm::new(key);
                let nonce = Nonce::from_slice(nonce_bytes);

                cipher
                    .decrypt(nonce, ciphertext)
                    .map_err(|e| anyhow!("分块数据解密失败: {e:?}"))
            })
            .await
            .context("解密任务本身发生错误，可以尝试重试")??;

            // 删除 R2 中已下载的分块
            r2::delete_object(&chunk_id.to_string()).await?;

            {
                let mut md = self.metadata.lock().await;

                // 越界保护
                if chunk_id >= md.total_chunks {
                    bail!("块索引 {} 超出范围 (总数: {})", chunk_id, md.total_chunks);
                }

                let offset = chunk_id * md.chunk_size;

                {
                    let mut file = self.file.lock().await;
                    file.seek(SeekFrom::Start(offset))
                        .context("移动文件指针失败")?;
                    file.write_all(&plain_data)
                        .context(format!("写入分块 #{chunk_id} 失败"))?;
                    file.flush().context("刷新文件缓冲区失败")?;
                }

                // 更新 bitmap + 保存 metadata
                md.chunk_bitmap.mark_completed(chunk_id);
                md.save()?;
            }

            Ok(())
        }
        .await;
        match result {
            Ok(_) => ChunkResult::Success(chunk_id),
            Err(e) => ChunkResult::Failure(chunk_id, e),
        }
    }

    pub async fn finalize(&self) -> Result<()> {
        let md = self.metadata.lock().await;

        if md.completed_chunks() != md.total_chunks() {
            bail!(
                "尝试完成下载，但还有未完成的分块: {}/{}",
                md.completed_chunks(),
                md.total_chunks()
            );
        }

        {
            let mut file = self.file.lock().await;
            file.flush().context("刷新文件缓冲区失败")?;
        }

        let cwd: PathBuf = env::current_dir().unwrap();
        let temp_path = cwd.join(format!("{}.gmf", md.xxh3));
        let final_path = cwd.join(&md.name);

        // 使用安全方式获取一个不会覆盖任何文件的最终路径
        let safe_path = find_available_filename(&final_path);

        ui::log_debug(&format!(
            "重命名临时文件 '{}' → '{}'",
            temp_path.display(),
            safe_path.display()
        ));

        std::fs::rename(&temp_path, &safe_path).with_context(|| {
            format!(
                "重命名文件失败: {} → {}",
                temp_path.display(),
                safe_path.display()
            )
        })?;

        md.remove()?;

        ui::log_debug("元数据文件已删除，下载会话清理完成");

        Ok(())
    }
}
