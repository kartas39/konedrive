//! QuickXorHash: the one content hash Microsoft Graph guarantees for files on
//! personal and business drives alike.
//!
//! The published algorithm XORs byte `k` of the content into a 160-bit
//! circular register at bit offset `(11 * k) mod 160`, then XORs the content's
//! length, little-endian, into the register's last 8 bytes. The offset depends
//! only on `k mod 160`, and XOR is linear, so every byte is first folded into
//! one of 160 lanes — one XOR per byte, whatever the file's size — and the
//! lanes are placed into the register once, at the end.

use base64::Engine;

const WIDTH_BITS: usize = 160;
const SHIFT: usize = 11;
/// Bytes in a hash.
pub const LEN: usize = 20;

#[derive(Clone)]
pub struct QuickXor {
    lanes: [u8; WIDTH_BITS],
    length: u64,
}

impl Default for QuickXor {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickXor {
    pub fn new() -> Self {
        Self { lanes: [0; WIDTH_BITS], length: 0 }
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut lane = (self.length % WIDTH_BITS as u64) as usize;
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
mod tests {
    use super::*;

    fn hash(data: &[u8]) -> String {
        let mut h = QuickXor::new();
        h.update(data);
        h.finish_base64()
    }

    /// A literal transcription of the published algorithm, one bit at a time:
    /// byte `k` goes into a 160-bit circular register at bit `(11 * k) mod 160`,
    /// and the length, little-endian, is XORed into the last 8 bytes. Nothing
    /// in it is shared with the implementation under test.
    fn reference(data: &[u8]) -> [u8; 20] {
        let mut register = [0u8; 20];
        for (k, &byte) in data.iter().enumerate() {
            let offset = (k * 11) % 160;
            for bit in 0..8 {
                if byte >> bit & 1 == 1 {
                    let position = (offset + bit) % 160;
                    register[position / 8] ^= 1 << (position % 8);
                }
            }
        }
        for (i, byte) in (data.len() as u64).to_le_bytes().iter().enumerate() {
            register[12 + i] ^= byte;
        }
        register
    }

    /// Deterministic noise, so a failure reproduces.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn nothing_hashes_to_twenty_zero_bytes() {
        assert_eq!(hash(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn one_byte_lands_at_bit_zero_and_the_length_at_byte_twelve() {
        // 0x4a at offset 0 → byte 0 = 0x4a; length 1 → byte 12 = 0x01.
        assert_eq!(hash(&[0x4a]), "SgAAAAAAAAAAAAAAAQAAAAAAAAA=");
    }

    #[test]
    fn the_second_byte_lands_eleven_bits_further() {
        // 0xb5 → byte 0. 0xb4 = bits 2,4,5,7 → positions 13,15,16,18:
        // byte 1 gets bits 5,7 (0xa0), byte 2 bits 0,2 (0x05). Length 2 → byte 12.
        assert_eq!(hash(&[0xb5, 0xb4]), "taAFAAAAAAAAAAAAAgAAAAAAAAA=");
    }

    #[test]
    fn matches_the_bit_by_bit_transcription_on_every_length_up_to_a_thousand() {
        for len in 0..1000 {
            let data = noise(len, len as u64);
            let mut h = QuickXor::new();
            h.update(&data);
            assert_eq!(h.finish(), reference(&data), "length {len}");
        }
    }

    #[test]
    fn matches_the_transcription_on_a_megabyte() {
        let data = noise(1 << 20, 7);
        let mut h = QuickXor::new();
        h.update(&data);
        assert_eq!(h.finish(), reference(&data));
    }

    #[test]
    fn a_stream_in_pieces_hashes_like_the_whole() {
        let data = noise(100_003, 11);
        let mut whole = QuickXor::new();
        whole.update(&data);
        // Piece sizes that straddle the 160-byte period in every way.
        for sizes in [[1usize, 159, 160, 161], [7, 300, 13, 2], [160, 160, 160, 160]] {
            let mut pieces = QuickXor::new();
            let mut at = 0;
            let mut i = 0;
            while at < data.len() {
                let end = (at + sizes[i % sizes.len()]).min(data.len());
                pieces.update(&data[at..end]);
                at = end;
                i += 1;
            }
            assert_eq!(pieces.finish(), whole.finish(), "pieces {sizes:?}");
        }
    }

    #[test]
    fn base64_round_trips() {
        let h = {
            let mut h = QuickXor::new();
            h.update(b"konedrive");
            h
        };
        assert_eq!(decode_base64(&h.finish_base64()), Some(h.finish()));
        assert_eq!(decode_base64("not base64!"), None);
        assert_eq!(decode_base64("AAAA"), None, "three bytes are not a quickXorHash");
    }
}
