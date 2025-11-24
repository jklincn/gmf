use base64::{Engine as _, engine::general_purpose};
use bitvec::prelude::*;
use bytemuck;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone)]
pub struct ChunkBitmap {
    pub bits: BitVec<u64, Lsb0>,
}

impl ChunkBitmap {
    pub fn new(total_chunks: u64) -> Self {
        let bits: BitVec<u64, Lsb0> = BitVec::repeat(false, total_chunks as usize);
        Self { bits }
    }

    pub fn mark_completed(&mut self, chunk_id: u64) {
        self.bits.set(chunk_id as usize, true);
    }

    pub fn is_completed(&self, chunk_id: u64) -> bool {
        self.bits[chunk_id as usize]
    }

    pub fn all_completed(&self) -> bool {
        self.bits.all()
    }

    pub fn completed_count(&self) -> u64 {
        self.bits.count_ones() as u64
    }

    pub fn pending_count(&self) -> u64 {
        (self.bits.len() - self.bits.count_ones()) as u64
    }

    pub fn completed_ids(&self) -> Vec<u64> {
        self.bits.iter_ones().map(|idx| idx as u64).collect()
    }

    pub fn pending_ids(&self) -> Vec<u64> {
        self.bits.iter_zeros().map(|idx| idx as u64).collect()
    }
}

impl Serialize for ChunkBitmap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let words: &[u64] = self.bits.as_raw_slice();
        let raw: &[u8] = bytemuck::cast_slice(words);

        let b64 = general_purpose::STANDARD.encode(raw);
        serializer.serialize_str(&b64)
    }
}

impl<'de> Deserialize<'de> for ChunkBitmap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        let raw = general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)?;

        let words: &[u64] = bytemuck::cast_slice(&raw);

        let bits: BitVec<u64, Lsb0> = BitVec::from_slice(words);

        Ok(Self { bits })
    }
}
