"""A pure-Python, stdlib-only port of Microsoft's QuickXorHash, the one content hash Graph
guarantees for files on personal and business drives alike. This is the same algorithm as
``crates/konedrived/src/quickxor.rs`` (read that file for the description); this module exists
only so ``stress_uploads.py`` can compute the hash of a local file without shelling out to Rust
or adding a dependency.

The update step is, byte for byte, ``lanes[k % 160] ^= data[k]`` for a running counter ``k`` — a
fold of the stream into 160 buckets. That is exactly a bytewise XOR of 160-byte blocks once the
stream is aligned to a 160-byte boundary, so ``update`` XORs 160-byte blocks together as big
integers (fast, in C) instead of looping one byte at a time in Python, which would be far too
slow for the 30-60 MiB files this tool uploads. ``finish`` (called once, on a short 160-byte
array) is cheap enough to stay a plain bit-by-bit loop, matching the Rust source directly.

Self-checked at import time against the three known-answer vectors from
``crates/konedrived/src/quickxor.rs``'s test suite, plus a differential check of the fast path
above against a bit-by-bit reference, across split points that straddle the 160-byte period. A
transcription mistake here should fail loudly the first time this module is imported, not
silently produce hashes that happen to agree with each other and disagree with OneDrive.
"""

from __future__ import annotations

import base64

WIDTH_BITS = 160
SHIFT = 11
LEN = 20


class QuickXor:
    """Incremental QuickXorHash. Call :meth:`update` as many times as convenient, then
    :meth:`finish` or :meth:`finish_base64` once."""

    __slots__ = ("_lanes_int", "length")

    def __init__(self) -> None:
        self._lanes_int = 0  # 160 lanes, packed as one big-endian integer of 160 bytes.
        self.length = 0

    def update(self, data: bytes) -> None:
        if not data:
            return
        start = self.length % WIDTH_BITS
        # Pad so the buffer starts at lane 0: the padding bytes are zero, so XOR-ing them in is
        # a no-op, and every real byte lands at the same lane it would one at a time.
        padded = bytes(start) + bytes(data)
        pad_tail = (-len(padded)) % WIDTH_BITS
        if pad_tail:
            padded += bytes(pad_tail)
        acc = 0
        for i in range(0, len(padded), WIDTH_BITS):
            acc ^= int.from_bytes(padded[i : i + WIDTH_BITS], "big")
        self._lanes_int ^= acc
        self.length += len(data)

    def finish(self) -> bytes:
        lanes = self._lanes_int.to_bytes(WIDTH_BITS, "big")
        register = bytearray(LEN)
        for lane, value in enumerate(lanes):
            if value == 0:
                continue
            offset = (lane * SHIFT) % WIDTH_BITS
            for bit in range(8):
                if (value >> bit) & 1:
                    position = (offset + bit) % WIDTH_BITS
                    register[position // 8] ^= 1 << (position % 8)
        length_bytes = (self.length & 0xFFFFFFFFFFFFFFFF).to_bytes(8, "little")
        for i, byte in enumerate(length_bytes):
            register[LEN - 8 + i] ^= byte
        return bytes(register)

    def finish_base64(self) -> str:
        return base64.b64encode(self.finish()).decode("ascii")


def hash_bytes(data: bytes) -> str:
    h = QuickXor()
    h.update(data)
    return h.finish_base64()


def hash_file(path, chunk_size: int = 4 * 1024 * 1024) -> str:
    h = QuickXor()
    with open(path, "rb") as f:
        while True:
            chunk = f.read(chunk_size)
            if not chunk:
                break
            h.update(chunk)
    return h.finish_base64()


def _reference(data: bytes) -> bytes:
    """A literal bit-by-bit transcription, independent of the fast path in ``update``. Used
    only by the self-test below."""
    register = bytearray(LEN)
    for k, byte in enumerate(data):
        offset = (k * SHIFT) % WIDTH_BITS
        for bit in range(8):
            if (byte >> bit) & 1:
                position = (offset + bit) % WIDTH_BITS
                register[position // 8] ^= 1 << (position % 8)
    length_bytes = (len(data) & 0xFFFFFFFFFFFFFFFF).to_bytes(8, "little")
    for i, byte in enumerate(length_bytes):
        register[LEN - 8 + i] ^= byte
    return bytes(register)


def _noise(length: int, seed: int) -> bytes:
    """The same small LCG the Rust tests use, for reproducible pseudo-random content."""
    state = seed
    out = bytearray(length)
    for i in range(length):
        state = (state * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
        out[i] = (state >> 56) & 0xFF
    return bytes(out)


def _self_test() -> None:
    # The three known-answer vectors from crates/konedrived/src/quickxor.rs.
    vectors = [
        (b"", "AAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        (bytes([0x4A]), "SgAAAAAAAAAAAAAAAQAAAAAAAAA="),
        (bytes([0xB5, 0xB4]), "taAFAAAAAAAAAAAAAgAAAAAAAAA="),
    ]
    for data, expected in vectors:
        got = hash_bytes(data)
        if got != expected:
            raise AssertionError(
                f"quickxor.py self-test failed for {data!r}: got {got}, want {expected} "
                "(check this port against crates/konedrived/src/quickxor.rs)"
            )

    # Differential check of the fast (block-XOR) update() against the bit-by-bit reference,
    # across lengths and split points that straddle the 160-byte period in different ways —
    # the thing most likely to be wrong in a "fold into 160-byte blocks" rewrite.
    for length in (0, 1, 2, 3, 159, 160, 161, 319, 320, 321, 733):
        data = _noise(length, seed=length + 1)
        want = _reference(data)
        for splits in ([length], [1, length - 1], [159, 160, length - 319], [7, 300, 13]):
            splits = [s for s in splits if s > 0]
            if sum(splits) != length:
                continue
            h = QuickXor()
            at = 0
            for s in splits:
                h.update(data[at : at + s])
                at += s
            got = h.finish()
            if got != want:
                raise AssertionError(
                    f"quickxor.py self-test failed: length {length}, splits {splits}: "
                    f"got {got.hex()}, want {want.hex()}"
                )


_self_test()


if __name__ == "__main__":
    print("quickxor.py: self-test passed.")
