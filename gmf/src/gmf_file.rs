use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub const CHUNK_SIZE: u64 = 10 * 1024 * 1024; /// 每块固定 10 MB
const NONCE_SIZE: usize = 12;

const MAGIC: &[u8; 13] = b"gmf temp file";
const HEADER_SIZE: u64 = 24;

/// GMF 文件头
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Header {
    magic: [u8; 13],
    _padding: [u8; 3], // 添加 3 字节的填充，使后续的 u32 字段对齐
    chunk_count: u32,
    written_count: u32,
}

impl Header {
    // --- 修改点 4 ---
    // 更新序列化方法以匹配新的布局
    fn to_bytes(&self) -> [u8; HEADER_SIZE as usize] {
        let mut buf = [0u8; HEADER_SIZE as usize];
        // magic 占用 0..13
        buf[..13].copy_from_slice(&self.magic);
        // 索引 13..16 是填充，保持为 0 即可
        // chunk_count 占用 16..20
        buf[16..20].copy_from_slice(&self.chunk_count.to_le_bytes());
        // written_count 占用 20..24
        buf[20..24].copy_from_slice(&self.written_count.to_le_bytes());
        buf
    }

    // --- 修改点 5 ---
    // 更新反序列化方法以匹配新的布局
    fn read_from<R: Read>(mut r: R) -> std::io::Result<Self> {
        let mut buf = [0u8; HEADER_SIZE as usize];
        r.read_exact(&mut buf)?;

        // 验证 magic (现在是 13 字节)
        if &buf[..13] != MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid magic",
            ));
        }

        let mut c1 = [0u8; 4];
        let mut c2 = [0u8; 4];
        // 从新的偏移量读取
        c1.copy_from_slice(&buf[16..20]);
        c2.copy_from_slice(&buf[20..24]);

        Ok(Self {
            magic: *MAGIC,
            _padding: [0; 3], // 初始化填充字段
            chunk_count: u32::from_le_bytes(c1),
            written_count: u32::from_le_bytes(c2),
        })
    }
}

pub struct GmfFile {
    file: File,
    header: Header,
}

impl GmfFile {
    // ---------------- 创建 ----------------
    // --- 修改点 6 ---
    // 创建 Header 实例时需要初始化 _padding 字段
    pub fn create<P: AsRef<Path>>(path: P, chunk_count: u32) -> std::io::Result<Self> {
        let header = Header {
            magic: *MAGIC,
            _padding: [0; 3], // 初始化填充字段
            chunk_count,
            written_count: 0,
        };
        let mut file = File::create(path)?;
        file.write_all(&header.to_bytes())?;
        Ok(Self { file, header })
    }

    // ---------------- 打开并验证 ----------------
    pub fn verify_and_open<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let header = Header::read_from(&mut file)?;
        Ok(Self { file, header })
    }

    // ---------------- 判断块是否存在 ----------------
    #[inline]
    pub fn is_chunk_present(&self, idx: u32) -> bool {
        idx < self.header.written_count
    }

    // ---------------- 顺序写入块 ----------------
    pub fn write_chunk(&mut self, idx: u32, data: &[u8]) -> std::io::Result<()> {
        // 1. 只能写“下一个块”
        if idx != self.header.written_count {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "expected idx {}, got {} (out of order)",
                    self.header.written_count, idx
                ),
            ));
        }
        if idx >= self.header.chunk_count {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "exceeds declared chunk_count",
            ));
        }
        if data.len() as u64 > CHUNK_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "data larger than CHUNK_SIZE (10 MB)",
            ));
        }

        // 2. 追加写入
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(data)?;
        if data.len() as u64 != CHUNK_SIZE {
            self.file
                .write_all(&vec![0u8; (CHUNK_SIZE - data.len() as u64) as usize])?;
        }

        // 3. 更新 written_count 并回写头部
        self.header.written_count += 1;
        // --- 修改点 7 ---
        // 更新 written_count 字段的偏移量
        self.file.seek(SeekFrom::Start(20))?; // 新的偏移量是 20
        self.file
            .write_all(&self.header.written_count.to_le_bytes())?;
        self.file.flush()?;
        Ok(())
    }

    // 便利方法
    pub fn header(&self) -> &Header {
        &self.header
    }
}

// // `main` 函数无需任何修改，可以取消注释直接运行
// fn main() -> std::io::Result<()> {
//     let path = "demo_seq.gmf";

//     // 创建一个声明 5 块的文件
//     let mut gmf = GmfFile::create(path, 5)?;

//     // 顺序写 3 个块
//     for i in 0..3 {
//         let payload = vec![i as u8; 1024]; // 示例数据
//         gmf.write_chunk(i, &payload)?;
//     }

//     // 读取第 2 块
//     let mut gmf_reader = GmfFile::verify_and_open(path)?;
//     let chunk2 = gmf_reader.read_chunk(2)?;
//     assert_eq!(chunk2[0], 2);
//     println!("Chunk 2 read successfully, first byte is 2.");

//     // 再次打开验证
//     let gmf2 = GmfFile::verify_and_open(path)?;
//     println!(
//         "File re-opened. Header info: written_count = {}, chunk_count = {}",
//         gmf2.header().written_count,
//         gmf2.header().chunk_count
//     );
//     assert_eq!(gmf2.header().written_count, 3);
//     assert_eq!(gmf2.header().chunk_count, 5);

//     Ok(())
// }

pub fn chunk_handle(chunk_id: u32, data: &[u8], key_bytes: &String) -> anyhow::Result<()> {
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_bytes)
        .map_err(|e| anyhow::anyhow!("Base64 解码失败: {:?}", e))?;
    let (nonce_bytes, ciphertext) = data.split_at(NONCE_SIZE);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("解密失败: {:?}", e))?;

    Ok(())
}
