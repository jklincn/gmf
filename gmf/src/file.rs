use crate::r2::{self, delete_object};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose};
use gmf_common::NONCE_SIZE;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

const MAGIC: &[u8; 13] = b"gmf temp file";
const HEADER_SIZE: u64 = 32;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub magic: [u8; 13],
    _padding: [u8; 3],
    pub chunk_count: u64,
    pub written_count: u64,
}

impl Header {
    /// 将 Header 序列化为字节数组
    fn to_bytes(self) -> [u8; HEADER_SIZE as usize] {
        let mut buf = [0u8; HEADER_SIZE as usize];

        // magic: 0..13
        buf[0..13].copy_from_slice(&self.magic);

        // padding: 13..16

        buf[16..24].copy_from_slice(&self.chunk_count.to_le_bytes());

        buf[24..32].copy_from_slice(&self.written_count.to_le_bytes());

        buf
    }

    /// 从字节数组创建 Header
    fn from_bytes(bytes: [u8; HEADER_SIZE as usize]) -> Self {
        let mut magic = [0u8; 13];
        magic.copy_from_slice(&bytes[0..13]);

        let chunk_count_bytes: [u8; 8] = bytes[16..24].try_into().unwrap();
        let chunk_count = u64::from_le_bytes(chunk_count_bytes);

        let written_count_bytes: [u8; 8] = bytes[24..32].try_into().unwrap();
        let written_count = u64::from_le_bytes(written_count_bytes);

        Self {
            magic,
            _padding: [0; 3],
            chunk_count,
            written_count,
        }
    }
}

pub struct GMFFile {
    file: File,
    header: Header,
    pub source_filename: String,
    pub source_size: u64,
    pub source_sha256: String,
}

impl GMFFile {
    pub fn new(
        source_filename: &str,
        source_size: u64,
        source_sha256: &str,
        chunk_count: u64,
    ) -> Result<(Self, u64)> {
        let gmf_filename = PathBuf::from(format!(".{source_sha256}.gmf"));

        if gmf_filename.exists() {
            match Self::open_and_validate(
                &gmf_filename,
                chunk_count,
                source_filename,
                source_size,
                source_sha256,
            ) {
                Ok(gmf_file) => {
                    let completed_chunks = gmf_file.header.written_count;
                    println!("检测到未完成的下载任务，从分块 {completed_chunks} 继续。");
                    return Ok((gmf_file, completed_chunks));
                }
                Err(e) => {
                    println!("警告：发现旧的临时文件，但验证失败：{}。将重新创建。", e);
                    fs::remove_file(&gmf_filename).context("删除无效的临时文件失败")?;
                }
            }
        }

        // 如果文件不存在或验证失败，则创建新文件
        let gmf_file = Self::create(
            &gmf_filename,
            chunk_count,
            source_filename,
            source_size,
            source_sha256,
        )?;
        Ok((gmf_file, 0)) // 新文件，已完成 0 个
    }

    // 打开并验证现有文件 ---
    fn open_and_validate<P: AsRef<Path>>(
        path: P,
        expected_chunk_count: u64,
        source_filename: &str,
        source_size: u64,
        source_sha256: &str,
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

        // 验证1: Magic Number
        if &header.magic != MAGIC {
            bail!("文件格式不正确 (magic number 错误)");
        }

        // 验证2: 分块总数
        if header.chunk_count != expected_chunk_count {
            bail!(
                "分块总数不匹配 (文件记录: {}, 当前任务: {})",
                header.chunk_count,
                expected_chunk_count
            );
        }

        // 验证3: 已写入分块数是否合理
        if header.written_count > header.chunk_count {
            bail!(
                "文件头数据损坏 (已写入数 {} > 总数 {})",
                header.written_count,
                header.chunk_count
            );
        }

        Ok(Self {
            file,
            header,
            source_filename: source_filename.to_string(),
            source_size,
            source_sha256: source_sha256.to_string(),
        })
    }

    fn create<P: AsRef<Path>>(
        path: P,
        chunk_count: u64,
        source_filename: &str,
        source_size: u64,
        source_sha256: &str,
    ) -> Result<Self> {
        let header = Header {
            magic: *MAGIC,
            _padding: [0; 3],
            chunk_count,
            written_count: 0,
        };
        let mut file = File::create(path).context("创建 GMF 临时文件失败")?;
        file.write_all(&header.to_bytes())
            .context("写入 GMF 文件头失败")?;

        Ok(Self {
            file,
            header,
            source_filename: source_filename.to_string(),
            source_size,
            source_sha256: source_sha256.to_string(),
        })
    }

    pub fn write_chunk(&mut self, idx: u64, data: &[u8]) -> Result<()> {
        if idx != self.header.written_count {
            bail!(
                "写入顺序错误：期望写入块 {}, 但收到了块 {}",
                self.header.written_count,
                idx
            );
        }
        if idx >= self.header.chunk_count {
            bail!(
                "块索引 {} 超出范围 (总数: {})",
                idx,
                self.header.chunk_count
            );
        }

        self.file
            .seek(SeekFrom::End(0))
            .context("移动文件指针到末尾失败")?;
        self.file.write_all(data).context("写入分块数据失败")?;

        self.header.written_count += 1;
        self.file
            .seek(SeekFrom::Start(24))
            .context("移动文件指针到头部失败")?;
        self.file
            .write_all(&self.header.written_count.to_le_bytes())
            .context("更新已写入分块计数失败")?;
        self.file.flush().context("刷新文件缓冲区失败")?;
        Ok(())
    }
}

struct DecryptedChunk {
    id: u64,
    data: Vec<u8>,
}

#[derive(Debug)]
pub enum ChunkResult {
    Success(u64),
    Failure(u64, anyhow::Error),
}

pub struct GmfSession {
    gmf_file: Arc<Mutex<GMFFile>>,
    decrypted_tx: mpsc::Sender<DecryptedChunk>,
    writer_handle: JoinHandle<Result<()>>,
}

impl GmfSession {
    pub fn new(gmf_file: GMFFile, completed_chunks: u64) -> Self {
        let (decrypted_tx, decrypted_rx) = mpsc::channel(128);
        let gmf_file_arc = Arc::new(Mutex::new(gmf_file));

        let writer_handle = tokio::spawn(run_writer_task(
            gmf_file_arc.clone(),
            decrypted_rx,
            completed_chunks,
        ));

        Self {
            gmf_file: gmf_file_arc,
            decrypted_tx,
            writer_handle,
        }
    }

    pub fn handle_chunk(&self, chunk_id: u64, passphrase_b64: String) -> JoinHandle<ChunkResult> {
        let decrypted_tx = self.decrypted_tx.clone();

        tokio::spawn(async move {
            // 使用一个内部 async 块来执行所有操作，这样可以方便地使用 `?` 来传播错误。
            let result: Result<()> = async {
                // 1. 下载加密的分块数据
                let content = r2::get_object(&chunk_id.to_string())
                    .await
                    .with_context(|| format!("从 r2 获取分块 {chunk_id} 数据失败"))?;

                // 2. 解密分块
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
                        .map_err(|e| anyhow!("分块数据解密失败: {:?}", e))
                })
                .await
                .context("解密任务本身发生错误 (例如 panic)")??;

                delete_object(&chunk_id.to_string())
                    .await
                    .context("删除已下载的分块对象失败")?;

                // 3. 将解密后的数据发送给写入器任务
                decrypted_tx
                    .send(DecryptedChunk {
                        id: chunk_id,
                        data: plain_data,
                    })
                    .await
                    .map_err(|e| anyhow!("发送解密数据给写入器失败: {}", e))?;

                Ok(())
            }
            .await;

            // 根据内部 async 块的结果，返回一个 ChunkResult
            match result {
                Ok(_) => ChunkResult::Success(chunk_id),
                Err(e) => ChunkResult::Failure(chunk_id, e),
            }
        })
    }

    /// 等待所有已派发的任务完成，并关闭写入器。
    pub async fn wait_for_completion(self) -> Result<()> {
        // 1. 通知写入器不会再有新数据了
        drop(self.decrypted_tx);

        // 2. 等待写入器任务完成，确保所有分块都已写入磁盘
        println!("等待写入器任务完成所有缓冲写入...");
        match self.writer_handle.await {
            Ok(Ok(_)) => { /* 写入器成功退出 */ }
            Ok(Err(e)) => return Err(e), // 写入器任务内部返回错误
            Err(e) => return Err(anyhow!("写入器任务 panic: {}", e)), // 写入器任务本身 panic
        }

        // 3. 获取 GMFFile 的所有权，准备进行最终处理
        // Arc::try_unwrap 确保我们是 Arc 的唯一所有者，如果不是则表示有逻辑错误
        let gmf_mutex = Arc::try_unwrap(self.gmf_file)
            .map_err(|_| anyhow!("无法获取 GMFFile 的唯一所有权，可能存在悬空引用"))?;
        let gmf_file = gmf_mutex.into_inner();

        // 4. 在一个阻塞任务中执行所有同步的文件 I/O 操作
        tokio::task::spawn_blocking(move || finalize_and_verify_file(gmf_file))
            .await
            .context("文件最终化任务 panic")??; // 第一个?处理JoinError, 第二个?处理内部Result

        Ok(())
    }
}

fn finalize_and_verify_file(gmf_file: GMFFile) -> Result<()> {
    let temp_filename = format!(".{}.gmf", gmf_file.source_sha256);
    let target_filename = gmf_file.source_filename.clone();

    // 显式 drop 文件句柄，以便后续可以安全地进行截断和重命名
    drop(gmf_file.file);

    {
        // 使用新的作用域来管理文件句柄
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&temp_filename)
            .with_context(|| format!("无法重新打开临时文件 '{temp_filename}' 进行最终化"))?;

        let total_size = file.metadata()?.len();
        if total_size < HEADER_SIZE {
            bail!("临时文件大小异常，小于文件头大小");
        }
        let content_size = total_size - HEADER_SIZE;

        let mut buffer = vec![0; 8192];
        let mut read_pos = HEADER_SIZE;
        let mut write_pos = 0;
        while read_pos < total_size {
            let bytes_to_read = std::cmp::min(buffer.len() as u64, total_size - read_pos) as usize;
            file.seek(SeekFrom::Start(read_pos))?;
            file.read_exact(&mut buffer[..bytes_to_read])?;
            file.seek(SeekFrom::Start(write_pos))?;
            file.write_all(&buffer[..bytes_to_read])?;
            read_pos += bytes_to_read as u64;
            write_pos += bytes_to_read as u64;
        }

        file.set_len(content_size).context("截断临时文件失败")?;
    }

    println!("正在校验最终文件...");
    let final_file = File::open(&temp_filename)
        .with_context(|| format!("无法打开最终文件 '{temp_filename}' 进行校验"))?;

    let final_size = final_file.metadata()?.len();
    if final_size != gmf_file.source_size {
        bail!(
            "文件大小校验失败：期望大小 {}, 实际大小 {}",
            gmf_file.source_size,
            final_size
        );
    }

    let sha256 = gmf_common::calc_sha256(temp_filename.as_ref())
        .with_context(|| format!("计算最终文件 '{temp_filename}' 的 SHA256 失败"))?;

    if sha256 != gmf_file.source_sha256 {
        bail!(
            "文件 SHA256 校验失败：\n  期望值: {}\n  计算值: {}",
            gmf_file.source_sha256,
            sha256
        );
    }

    fs::rename(&temp_filename, &target_filename)
        .with_context(|| format!("重命名文件从 '{temp_filename}' 到 '{target_filename}' 失败"))?;

    println!("文件 '{target_filename}' 已成功下载并校验！");
    Ok(())
}

// 后台“写入器”任务
async fn run_writer_task(
    gmf_file_arc: Arc<Mutex<GMFFile>>,
    mut decrypted_rx: mpsc::Receiver<DecryptedChunk>,
    completed_chunks: u64,
) -> Result<()> {
    let mut buffer = BTreeMap::new();
    let mut next_chunk_to_write = completed_chunks;

    while let Some(chunk) = decrypted_rx.recv().await {
        // 如果收到的块是已经写入过的，直接丢弃
        if chunk.id < next_chunk_to_write {
            println!("丢弃已处理过的分块 {}", chunk.id);
            continue;
        }

        buffer.insert(chunk.id, chunk.data);

        while let Some(data_to_write) = buffer.remove(&next_chunk_to_write) {
            let mut gmf = gmf_file_arc.lock().await;
            gmf.write_chunk(next_chunk_to_write, &data_to_write)?;
            drop(gmf);
            next_chunk_to_write += 1;
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
