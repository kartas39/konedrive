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

/// Pieces hashed at their offsets, in any order, combine into the hash of
/// the whole: what a download in parallel parts checks.
#[test]
fn pieces_hashed_at_their_offsets_combine_into_the_whole() {
    let data = noise(100_003, 13);
    let mut whole = QuickXor::new();
    whole.update(&data);
    // Boundaries that fall on and off the 160-byte period.
    let bounds = [0usize, 1, 161, 4_000, 4_160, 50_001, 100_003];
    let mut pieces: Vec<QuickXor> = bounds
        .windows(2)
        .map(|w| {
            let mut h = QuickXor::at(w[0] as u64);
            h.update(&data[w[0]..w[1]]);
            h
        })
        .collect();
    pieces.reverse();
    let mut combined = QuickXor::new();
    for piece in &pieces {
        combined.combine(piece);
    }
    assert_eq!(combined.finish(), whole.finish());
    assert_eq!(combined.finish(), reference(&data));
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
