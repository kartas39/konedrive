//! QuickXorHash: the one content hash Microsoft Graph guarantees for files on
//! personal and business drives alike.
//!
//! The published algorithm XORs byte `k` of the content into a 160-bit
//! circular register at bit offset `(11 * k) mod 160`, then XORs the content's
//! length, little-endian, into the register's last 8 bytes. The offset depends
//! only on `k mod 160`, and XOR is linear, so every byte is first folded into
//! one of 160 lanes — one XOR per byte, whatever the file's size — and the
//! lanes are placed into the register once, at the end.
//!
//! The same linearity lets a file be hashed in pieces, in any order: a piece
//! hashed [`at`](QuickXor::at) its offset in the file folds its bytes into
//! the lanes they belong to in the whole, and the pieces
//! [`combine`](QuickXor::combine) into the whole file's hash.

use base64::Engine;

const WIDTH_BITS: usize = 160;
const SHIFT: usize = 11;
/// Bytes in a hash.
pub const LEN: usize = 20;

#[derive(Clone)]
pub struct QuickXor {
    lanes: [u8; WIDTH_BITS],
    length: u64,
    /// Where in the file the first byte given to [`update`](Self::update) lies.
    start: u64,
}

impl Default for QuickXor {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickXor {
    pub fn new() -> Self {
        Self::at(0)
    }

    /// A hasher of the bytes that start at `offset` in the file: a piece of it,
    /// to be [`combine`](Self::combine)d with the others.
    pub fn at(offset: u64) -> Self {
        Self { lanes: [0; WIDTH_BITS], length: 0, start: offset }
    }

    /// Adds the hash of another piece of the same file; the pieces must not
    /// overlap. Once every byte of the file is in, [`finish`](Self::finish)
    /// gives the whole file's hash, whatever order the pieces came in.
    pub fn combine(&mut self, other: &QuickXor) {
        for (lane, value) in self.lanes.iter_mut().zip(other.lanes.iter()) {
            *lane ^= value;
        }
        self.length += other.length;
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut lane = ((self.start + self.length) % WIDTH_BITS as u64) as usize;
        for &byte in data {
            self.lanes[lane] ^= byte;
            lane += 1;
            if lane == WIDTH_BITS {
                lane = 0;
            }
        }
        self.length += data.len() as u64;
    }

    pub fn finish(&self) -> [u8; LEN] {
        let mut register = [0u8; LEN];
        for (lane, &value) in self.lanes.iter().enumerate() {
            if value == 0 {
                continue;
            }
            let offset = (lane * SHIFT) % WIDTH_BITS;
            for bit in 0..8 {
                if value >> bit & 1 == 1 {
                    let position = (offset + bit) % WIDTH_BITS;
                    register[position / 8] ^= 1 << (position % 8);
                }
            }
        }
        for (i, byte) in self.length.to_le_bytes().iter().enumerate() {
            register[LEN - 8 + i] ^= byte;
        }
        register
    }

    /// As Graph spells it in `file.hashes.quickXorHash`.
    pub fn finish_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.finish())
    }
}

/// A `quickXorHash` as Graph spells it, or `None` if it is not one.
pub fn decode_base64(value: &str) -> Option<[u8; LEN]> {
    base64::engine::general_purpose::STANDARD.decode(value).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests;
