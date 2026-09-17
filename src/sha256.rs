//! SHA-256, because an image has to be checked before it is believed.
//!
//! Hand-written for the same reason the CRCs in [`crate::config`] and the
//! sensor drivers are: it is a fixed, published algorithm with canonical test
//! vectors, it needs no allocation, and it keeps the dependency list of a
//! `no_std` firmware short. The one thing it must be is *streaming* — an image
//! is 740 KB against 400 KB of SRAM, so the digest is computed as the bytes go
//! past on their way into flash, never over a buffer holding all of them.
//!
//! What it is for: the offer on the broker names a digest, and an image whose
//! bytes do not produce it is discarded before `otadata` is touched. That
//! catches a truncated download, a proxy that helpfully returned an error page,
//! and a file rebuilt on the server while a node was fetching it. It is
//! integrity, not authenticity — see the threat model in `docs/ota.md`.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A streaming SHA-256. Feed it [`update`](Sha256::update) as often as you
/// like; the block buffer absorbs whatever does not land on a 64-byte boundary.
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    /// How many bytes of `block` are filled.
    filled: usize,
    /// Total message length, needed for the padding at the end.
    len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub const fn new() -> Self {
        Self {
            state: H0,
            block: [0u8; 64],
            filled: 0,
            len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                let block = self.block;
                self.compress(&block);
                self.filled = 0;
            }
        }
    }

    /// The digest, consuming the hasher. Padding is the standard one: a `0x80`
    /// byte, zeros, and the message length in bits as a big-endian u64.
    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        // The length must land in the last 8 bytes of a block; pad with zeros
        // until only those 8 are left, rolling over into a second block if the
        // 0x80 landed too late for that.
        while self.filled != 56 {
            self.update(&[0x00]);
        }
        // `update` would recurse into the padding branch; write the length in
        // directly instead.
        self.block[56..].copy_from_slice(&bits.to_be_bytes());
        let block = self.block;
        self.compress(&block);

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (s, v) in self
            .state
            .iter_mut()
            .zip([a, b, c, d, e, f, g, h].into_iter())
        {
            *s = s.wrapping_add(v);
        }
    }
}

/// Parse a 64-character lowercase-or-uppercase hex digest, as it arrives in an
/// OTA offer. Anything that is not exactly 32 bytes of hex is refused rather
/// than padded — a half-parsed digest would compare unequal against everything
/// and look like a corrupt download.
pub fn parse_hex_digest(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

const fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: &[u8; 32]) -> heapless::String<64> {
        let mut s = heapless::String::new();
        for b in d {
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{b:02x}"));
        }
        s
    }

    fn digest(data: &[u8]) -> heapless::String<64> {
        let mut h = Sha256::new();
        h.update(data);
        hex(&h.finish())
    }

    #[test]
    fn canonical_vectors() {
        // The three vectors every SHA-256 implementation is checked against.
        assert_eq!(
            digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_million_a_s() {
        // The long vector, which is the one that catches a broken length
        // counter or a padding block that does not roll over.
        let mut h = Sha256::new();
        for _ in 0..1_000 {
            h.update(&[b'a'; 1_000]);
        }
        assert_eq!(
            hex(&h.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn streaming_matches_one_shot() {
        // The property the image writer depends on: how the bytes are split
        // must not change the digest. Splits chosen around the 64-byte block
        // boundary, where a buffering bug would live.
        let data: heapless::Vec<u8, 200> = (0..200u16).map(|i| (i % 251) as u8).collect();
        let once = digest(&data);
        for split in [1usize, 63, 64, 65, 127, 128, 129, 199] {
            let mut h = Sha256::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(hex(&h.finish()), once, "split at {split}");
        }
    }

    #[test]
    fn hex_digests_round_trip() {
        let d = [0xABu8; 32];
        let text = hex(&d);
        assert_eq!(parse_hex_digest(&text), Some(d));
        assert_eq!(parse_hex_digest(&text.to_uppercase()), Some(d));
    }

    #[test]
    fn a_digest_that_is_not_one_is_refused() {
        // Every one of these has been seen in a config file somewhere.
        assert_eq!(parse_hex_digest(""), None);
        assert_eq!(parse_hex_digest("abc"), None);
        assert_eq!(parse_hex_digest(&"a".repeat(63)), None);
        assert_eq!(parse_hex_digest(&"a".repeat(65)), None);
        // 64 characters, but not hex.
        assert_eq!(parse_hex_digest(&"g".repeat(64)), None);
        // A digest with a "0x" on the front is 64 characters of nearly-hex.
        assert_eq!(parse_hex_digest(&format!("0x{}", "a".repeat(62))), None);
    }
}
