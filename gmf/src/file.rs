use crate::ui;
use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose};
use gmf_common::{chunk::ChunkBitmap, consts::NONCE_SIZE, r2, utils::app_dir};
use serde::{Deserialize, Serialize};
use std::env;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinHandle,
};
use xxhash_rust::xxh3::xxh3_64;

const MAGIC: &[u8; 16] = b"gmf temp file\0\0\0";
// 16 (magic) + 8 (file_size) + 8 (total_chunks) + 8 (completed_chunks) = 40
const HEADER_SIZE: u64 = 40;

/// 文件数据结构
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub magic: [u8; 16],
    pub file_size: u64,
    pub total_chunks: u64,
    pub completed_chunks: u64,
}

impl Header {
    pub fn new(file_size: u64, total_chunks: u64) -> Self {
        Self {
            magic: *MAGIC,
            file_size,
            total_chunks,
            completed_chunks: 0,
        }
    }

    /// 将 Header 序列化为字节数组
    pub fn to_bytes(self) -> [u8; HEADER_SIZE as usize] {
        let mut buf = [0u8; HEADER_SIZE as usize];
        buf[0..16].copy_from_slice(&self.magic);
        buf[16..24].copy_from_slice(&self.file_size.to_le_bytes());
        buf[24..32].copy_from_slice(&self.total_chunks.to_le_bytes());
        buf[32..40].copy_from_slice(&self.completed_chunks.to_le_bytes());
        buf
    }

    /// 从字节数组创建 Header
    pub fn from_bytes(bytes: [u8; HEADER_SIZE as usize]) -> Self {
        let mut magic = [0u8; 16];
        magic.copy_from_slice(&bytes[0..16]);
        let file_size = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let total_chunks = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let completed_chunks = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        Self {
            magic,
            file_size,
            total_chunks,
            completed_chunks,
        }
    }
}

// 文件上下文
pub struct GMFFile {
    file: File,
    header: Header,
    pub file_name: String,
    pub temp_path: PathBuf,
}

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
    fn path(&self) -> PathBuf {
        app_dir().join(format!("{}.json", self.xxh3))
    }
    pub fn open(&self) -> Result<()>{
        let cwd: PathBuf = env::current_dir().unwrap();
        let metadata_path = self.path();
        if metadata_path.exists() {
            let metadata_content = fs::read_to_string(&metadata_path)
                .with_context(|| format!("读取元数据文件 '{}' 失败", metadata_path.display()))?;
            let metadata: Metadata = serde_json::from_str(&metadata_content)
                .with_context(|| format!("解析元数据文件 '{}' 失败", metadata_path.display()))?;
            // 验证元数据是否匹配
            if metadata.name != self.name
                || metadata.size != self.size
                || metadata.xxh3 != self.xxh3
                || metadata.local_path != self.local_path
                || metadata.remote_path != self.remote_path
                || metadata.chunk_size != self.chunk_size
                || metadata.total_chunks != self.total_chunks
            {
                bail!(
                    "元数据文件 '{}' 内容与当前下载任务不匹配，可能是旧的或损坏的文件",
                    metadata_path.display()
                );
            }
        }
        Ok(())
    }
    pub fn remove(&self) -> Result<()> {
        let metadata_path = self.path();
        if metadata_path.exists() {
            fs::remove_file(&metadata_path)
                .with_context(|| format!("删除元数据文件 '{}' 失败", metadata_path.display()))?;
        }
        Ok(())
    }
}

impl GMFFile {
    pub fn new(
        name: &str,
        size: u64,
        xxh3: &str,
        remote_path: &str,
        chunk_size: u64,
        total_chunks: u64,
    ) -> Result<()> {
        let cwd: PathBuf = env::current_dir().unwrap();
        let metadata_path = app_dir().join(format!("{}.json", xxh3));
        if metadata_path.exists() {
            let metadata_content = fs::read_to_string(&metadata_path)
                .with_context(|| format!("读取元数据文件 '{}' 失败", metadata_path.display()))?;
            let metadata: Metadata = serde_json::from_str(&metadata_content)
                .with_context(|| format!("解析元数据文件 '{}' 失败", metadata_path.display()))?;
            // 验证元数据是否匹配
            if metadata.name != name
                || metadata.size != size
                || metadata.remote_path != PathBuf::from(remote_path)
                || metadata.chunk_size != chunk_size
                || metadata.total_chunks != total_chunks
            {
                bail!(
                    "元数据文件 '{}' 内容与当前下载任务不匹配，可能是旧的或损坏的文件",
                    metadata_path.display()
                );
            }
            ui::log_info("继续未完成的下载任务");
        } else {
            let metadata = Metadata {
                name: name.to_string(),
                size,
                xxh3: xxh3.to_string(),
                local_path: cwd.join(name),
                remote_path: PathBuf::from(remote_path),
                chunk_size,
                total_chunks,
                chunk_bitmap: ChunkBitmap::new(total_chunks),
            };
            let metadata_content = serde_json::to_string_pretty(&metadata)
                .with_context(|| format!("序列化元数据失败"))?;
            fs::write(&metadata_path, metadata_content)
                .with_context(|| format!("写入元数据文件 '{}' 失败", metadata_path.display()))?;
            ui::log_debug(&format!("创建新的元数据文件: {}", metadata_path.display()));
        }
        Ok(())
    }

    pub fn new_or_resume(
        file_name: &str,
        file_size: u64,
        total_chunks: u64,
    ) -> Result<(Self, u64)> {
        let combined_string = format!("{file_name}:{file_size}");
        let combined_hash = xxh3_64(combined_string.as_bytes());
        let temp_filename = PathBuf::from(format!(".{combined_hash:x}.gmf"));

        if temp_filename.exists() {
            match Self::open(&temp_filename, file_size, total_chunks, file_name) {
                Ok(gmf_file) => {
                    let completed_chunks = gmf_file.header.completed_chunks;
                    ui::log_info("➡️ 继续未完成的下载任务");
                    return Ok((gmf_file, completed_chunks));
                }
                Err(e) => {
                    ui::log_warn(&format!("发现旧的临时文件，但验证失败：{e}。将重新创建。"));
                    fs::remove_file(&temp_filename).context("删除无效的临时文件失败")?;
                }
            }
        }

        ui::log_debug(&format!("创建新的临时文件: {}", temp_filename.display()));
        let gmf_file = Self::create(&temp_filename, file_size, total_chunks, file_name)?;
        Ok((gmf_file, 0))
    }

    pub fn total_chunks(&self) -> u64 {
        self.header.total_chunks
    }

    #[allow(unused)]
    pub fn file_size(&self) -> u64 {
        self.header.file_size
    }

    #[allow(unused)]
    pub fn completed_chunks(&self) -> u64 {
        self.header.completed_chunks
    }

    fn open<P: AsRef<Path>>(
        path: P,
        expected_file_size: u64,
        expected_total_chunks: u64,
        file_name: &str,
    ) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .context("打开现有 GMF 临时文件失败")?;

        let mut header_bytes = [0u8; HEADER_SIZE as usize];
        file.read_exact(&mut header_bytes)
            .context("读取 GMF 文件头失败")?;

        let header = Header::from_bytes(header_bytes);

        if &header.magic != MAGIC {
            bail!("文件格式不正确 (magic number 错误)");
        }
        if header.file_size != expected_file_size {
            bail!(
                "文件总大小不匹配 (文件记录: {}, 当前任务: {})",
                header.file_size,
                expected_file_size
            );
        }
        if header.total_chunks != expected_total_chunks {
            bail!(
                "分块总数不匹配 (文件记录: {}, 当前任务: {})",
                header.total_chunks,
                expected_total_chunks
            );
        }
        if header.completed_chunks > header.total_chunks {
            bail!(
                "文件头数据损坏 (已写入数 {} > 总数 {})",
                header.completed_chunks,
                header.total_chunks
            );
        }

        Ok(Self {
            file,
            header,
            file_name: file_name.to_string(),
            temp_path: path.as_ref().to_path_buf(),
        })
    }

    fn create<P: AsRef<Path>>(
        path: P,
        file_size: u64,
        total_chunks: u64,
        file_name: &str,
    ) -> Result<Self> {
        let header = Header::new(file_size, total_chunks);

        let mut file = File::create(path.as_ref()).context("创建 GMF 临时文件失败")?;
        file.write_all(&header.to_bytes())
            .context("写入 GMF 文件头失败")?;

        Ok(Self {
            file,
            header,
            file_name: file_name.to_string(),
            temp_path: path.as_ref().to_path_buf(),
        })
    }

    pub fn write_chunk(&mut self, idx: u64, data: &[u8]) -> Result<()> {
        if idx != self.header.completed_chunks {
            bail!(
                "写入顺序错误：期望写入块 {}, 但收到了块 {}",
                self.header.completed_chunks,
                idx
            );
        }
        if idx >= self.header.total_chunks {
            bail!(
                "块索引 {} 超出范围 (总数: {})",
                idx,
                self.header.total_chunks
            );
        }

        // 定位到文件末尾，追加新数据
        self.file
            .seek(SeekFrom::End(0))
            .context("移动文件指针到末尾失败")?;
        self.file.write_all(data).context("写入分块数据失败")?;

        // 更新内存中的 header
        self.header.completed_chunks += 1;

        // 定位回文件开头，覆盖写入更新后的整个 header
        self.file
            .seek(SeekFrom::Start(0))
            .context("移动文件指针到头部失败")?;
        self.file
            .write_all(&self.header.to_bytes())
            .context("更新文件头失败")?;

        // 确保写入磁盘
        self.file.flush().context("刷新文件缓冲区失败")?;
        Ok(())
    }
}

struct DecryptedChunk {
    id: u64,
    data: Vec<u8>,
    pub permit: Option<OwnedSemaphorePermit>,
}

#[derive(Debug)]
pub enum ChunkResult {
    Success(u64),
    Timeout(u64),
    Failure(u64, anyhow::Error),
}

pub struct GmfSession {
    gmf_file: Arc<Mutex<GMFFile>>,
    decrypted_tx: mpsc::Sender<DecryptedChunk>,
    writer_handle: JoinHandle<Result<()>>,
    buffer_semaphore: Arc<Semaphore>,
    pub total_chunks: u64,
}

impl GmfSession {
    pub fn new(gmf_file: GMFFile, completed_chunks: u64) -> Self {
        let (decrypted_tx, decrypted_rx) = mpsc::channel(128);
        let buffer_semaphore = Arc::new(Semaphore::new(20));

        let total_chunks = gmf_file.total_chunks();
        let gmf_file_arc = Arc::new(Mutex::new(gmf_file));

        let writer_handle = tokio::spawn(writer(
            gmf_file_arc.clone(),
            decrypted_rx,
            completed_chunks,
            total_chunks,
        ));

        Self {
            gmf_file: gmf_file_arc,
            decrypted_tx,
            writer_handle,
            buffer_semaphore,
            total_chunks,
        }
    }

    /// 负责下载分块与解密，写入交由写入器操作
    pub async fn handle_chunk(
        &self,
        chunk_id: u64,
        passphrase_b64: String,
        retry: bool,
    ) -> ChunkResult {
        let permit = if retry {
            ui::log_debug(&format!(
                "分块 #{chunk_id} 是重试任务，跳过获取信号量许可。"
            ));
            None
        } else {
            match self.buffer_semaphore.clone().acquire_owned().await {
                Ok(p) => Some(p),
                Err(_) => {
                    return ChunkResult::Failure(
                        chunk_id,
                        anyhow!("内部错误: 无法获取信号量许可，会话已关闭"),
                    );
                }
            }
        };

        ui::log_debug(&format!("分块 #{chunk_id} 开始处理"));
        let decrypted_tx = self.decrypted_tx.clone();

        let result: anyhow::Result<()> = async {
            let start_time = std::time::Instant::now();

            // 下载分块
            let content = match r2::get_object(&chunk_id.to_string()).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    // 只有在非重试模式下才检查是否为超时错误
                    if !retry {
                        let error_msg = format!("{e:?}");
                        ui::log_debug(&format!("分块 #{chunk_id} 下载失败: {error_msg}"));
                        let is_timeout_error = error_msg.contains("Operation timed out");

                        if is_timeout_error {
                            return Err(anyhow!("TIMEOUT_ERROR:{}", e));
                        }
                    }
                    return Err(e.context(format!("分块 #{chunk_id} 下载失败（已重试）")));
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
            r2::delete_object(&chunk_id.to_string())
                .await
                .context("删除已下载的分块对象失败")?;

            // 将解密后分块发送到写入器
            decrypted_tx
                .send(DecryptedChunk {
                    id: chunk_id,
                    data: plain_data,
                    permit,
                })
                .await
                .map_err(|e| anyhow!("内部错误: 发送解密数据给写入器失败: {e}"))?;

            Ok(())
        }
        .await;

        match result {
            Ok(_) => ChunkResult::Success(chunk_id),
            Err(e) => {
                let error_msg = e.to_string();
                // 检查是否为超时错误标记
                if error_msg.starts_with("TIMEOUT_ERROR:") {
                    ChunkResult::Timeout(chunk_id)
                } else {
                    ChunkResult::Failure(chunk_id, e)
                }
            }
        }
    }

    /// 等待所有已派发的任务完成，并关闭写入器。
    pub async fn wait_for_completion(self) -> Result<()> {
        // 1) 关闭发送端，让 writer 知道不会再有新的 chunk 传来。
        drop(self.decrypted_tx);

        // 2) 关闭信号量，这样任何还在等待 acquire 的任务都会立即出错并退出。
        self.buffer_semaphore.close();

        // 3) 等待 writer 任务处理完所有已在通道中的数据并退出。
        match self.writer_handle.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(anyhow!("内部错误: 写入器任务 panic: {}", e)),
        }

        // 4) 取回 GMFFile 的所有权
        let gmf_mutex = Arc::try_unwrap(self.gmf_file)
            .map_err(|_| anyhow!("内部错误:无法获取 GMFFile 的唯一所有权，可能存在悬空引用"))?;
        let gmf_file = gmf_mutex.into_inner();

        // 5) 在阻塞线程池里处理最终文件
        tokio::task::spawn_blocking(move || {
            let temp_path = &gmf_file.temp_path;
            let target_filename = &gmf_file.file_name;

            // 显式 drop，确保后续可以截断/重命名
            drop(gmf_file.file);

            {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(temp_path)
                    .with_context(|| format!("无法重新打开临时文件 '{temp_path:?}' 进行最终化"))?;

                let total_size = file.metadata()?.len();
                if total_size < HEADER_SIZE {
                    bail!("临时文件大小异常，小于文件头大小");
                }
                let content_size = total_size - HEADER_SIZE;

                let mut buffer = vec![0; 8192];
                let mut read_pos = HEADER_SIZE;
                let mut write_pos = 0;
                while read_pos < total_size {
                    let bytes_to_read =
                        std::cmp::min(buffer.len() as u64, total_size - read_pos) as usize;
                    file.seek(SeekFrom::Start(read_pos))?;
                    file.read_exact(&mut buffer[..bytes_to_read])?;
                    file.seek(SeekFrom::Start(write_pos))?;
                    file.write_all(&buffer[..bytes_to_read])?;
                    read_pos += bytes_to_read as u64;
                    write_pos += bytes_to_read as u64;
                }

                file.set_len(content_size).context("截断临时文件失败")?;
            }

            fs::rename(temp_path, target_filename).with_context(|| {
                format!("重命名文件从 '{temp_path:?}' 到 '{target_filename}' 失败")
            })?;

            Ok(())
        })
        .await
        .context("内部错误: 文件最终处理失败")??;

        Ok(())
    }
}

// 后台“写入器”任务
async fn writer(
    gmf_file_arc: Arc<Mutex<GMFFile>>,
    mut decrypted_rx: mpsc::Receiver<DecryptedChunk>,
    completed_chunks: u64,
    total_chunks: u64,
) -> Result<()> {
    let mut buffer: BTreeMap<u64, (Vec<u8>, Option<OwnedSemaphorePermit>)> = BTreeMap::new();
    let mut next_chunk_to_write = completed_chunks;
    let mut remaining_chunks = total_chunks - completed_chunks;

    while let Some(chunk) = decrypted_rx.recv().await {
        // 如果收到的块是已经写入过的，直接丢弃
        if chunk.id < next_chunk_to_write {
            ui::log_warn(&format!("收到已处理过的分块 #{}，直接丢弃", chunk.id));
            continue;
        }

        // 将数据和许可一起存入缓冲区
        buffer.insert(chunk.id, (chunk.data, chunk.permit));

        // 尝试按顺序写入所有已缓冲的块
        while let Some(data_to_write_tuple) = buffer.remove(&next_chunk_to_write) {
            let (data_to_write, permit_to_release) = data_to_write_tuple;
            {
                let mut gmf = gmf_file_arc.lock().await;
                gmf.write_chunk(next_chunk_to_write, &data_to_write)?;
            }
            ui::log_debug(&format!("分块 #{next_chunk_to_write} 成功写入"));
            ui::update_download();
            next_chunk_to_write += 1;
            remaining_chunks -= 1;
            if remaining_chunks == 0 {
                ui::finish_download();
            }
            if let Some(permit) = permit_to_release {
                drop(permit);
            }
        }
    }

    if !buffer.is_empty() {
        let missing_chunks: Vec<String> = buffer.keys().map(|k| k.to_string()).collect();
        bail!(
            "会话结束，但以下分块已下载但未写入 (可能因缺少前序块): {}",
            missing_chunks.join(", ")
        );
    }
    Ok(())
}
