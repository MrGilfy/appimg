//! MD4, the strong checksum zsync uses for a block.

/// The MD4 of a slice, which is what a zsync table holds truncated to its
/// `checksum_bytes` for every block of the complete file.
pub fn md4(data: &[u8]) -> [u8; 16] {
    let mut digest = Md4::new();
    digest.update(data);
    digest.finish()
}

/// MD4 as in RFC 1320. zsync uses it as the strong checksum of a block, to
/// settle whether a rolling checksum hit is a real match. It is used here for
/// nothing else: MD4 is long broken as a hash, but the format is what it is,
/// and a block that passes it still has to pass the SHA-1 of the whole file.
pub struct Md4 {
    state: [u32; 4],
    block: [u8; 64],
    used: usize,
    length: u64,
}

impl Md4 {
    pub fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            block: [0u8; 64],
            used: 0,
            length: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);

        while !data.is_empty() {
            let space = 64 - self.used;
            let take = space.min(data.len());
            self.block[self.used..self.used + take].copy_from_slice(&data[..take]);
            self.used += take;
            data = &data[take..];

            if self.used == 64 {
                let block = self.block;
                self.compress(&block);
                self.used = 0;
            }
        }
    }

    /// The digest, sixteen bytes. Unlike the SHA-1 above this hands back the
    /// bytes rather than hex: what compares them is the block table, which is
    /// binary.
    pub fn finish(mut self) -> [u8; 16] {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        // The length has to land in the last eight bytes of a block.
        while self.used != 56 {
            self.update(&[0]);
        }
        let mut last = self.block;
        // MD4 is little endian throughout, where SHA-1 is big endian.
        last[56..].copy_from_slice(&bits.to_le_bytes());
        self.compress(&last);

        let mut out = [0u8; 16];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut words = [0u32; 16];
        for (word, chunk) in words.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*chunk);
        }

        let [mut a, mut b, mut c, mut d] = self.state;

        // Round one, in groups of four: F, no constant, shifts 3, 7, 11, 19.
        for group in 0..4 {
            let k = group * 4;
            a = round(a, f(b, c, d), words[k], 0, 3);
            d = round(d, f(a, b, c), words[k + 1], 0, 7);
            c = round(c, f(d, a, b), words[k + 2], 0, 11);
            b = round(b, f(c, d, a), words[k + 3], 0, 19);
        }

        // Round two: G, the square root of two, shifts 3, 5, 9, 13, and the
        // words taken in columns rather than rows.
        for group in 0..4 {
            a = round(a, g(b, c, d), words[group], SQRT2, 3);
            d = round(d, g(a, b, c), words[group + 4], SQRT2, 5);
            c = round(c, g(d, a, b), words[group + 8], SQRT2, 9);
            b = round(b, g(c, d, a), words[group + 12], SQRT2, 13);
        }

        // Round three: H, the square root of three, shifts 3, 9, 11, 15, and
        // the words in the order 0, 8, 4, 12, 2, 10, 6, 14, ...
        for group in [0, 2, 1, 3] {
            a = round(a, h(b, c, d), words[group], SQRT3, 3);
            d = round(d, h(a, b, c), words[group + 8], SQRT3, 9);
            c = round(c, h(d, a, b), words[group + 4], SQRT3, 11);
            b = round(b, h(c, d, a), words[group + 12], SQRT3, 15);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

impl Default for Md4 {
    fn default() -> Self {
        Self::new()
    }
}

/// The constants of rounds two and three: the first 32 bits of the fractional
/// parts of the square roots of two and three, as RFC 1320 has them.
const SQRT2: u32 = 0x5a82_7999;
const SQRT3: u32 = 0x6ed9_eba1;

fn f(x: u32, y: u32, z: u32) -> u32 {
    (x & y) | (!x & z)
}

fn g(x: u32, y: u32, z: u32) -> u32 {
    (x & y) | (x & z) | (y & z)
}

fn h(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}

fn round(value: u32, mixed: u32, word: u32, constant: u32, shift: u32) -> u32 {
    value.wrapping_add(mixed).wrapping_add(word).wrapping_add(constant).rotate_left(shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md4_hex(data: &[u8]) -> String {
        md4(data).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn hashes_the_rfc_1320_vectors() {
        assert_eq!(md4_hex(b""), "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(md4_hex(b"a"), "bde52cb31de33e46245e05fbdbd6fb24");
        assert_eq!(md4_hex(b"abc"), "a448017aaf21d8525fc10ae87aa6729d");
        assert_eq!(md4_hex(b"message digest"), "d9130a8164549fe818874806e1c7014b");
        assert_eq!(md4_hex(b"abcdefghijklmnopqrstuvwxyz"), "d79e1c308aa5bbcdeea8ed63df412da9");
        assert_eq!(
            md4_hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "043f8582f241db351ce627e153e7f0e4"
        );
        assert_eq!(
            md4_hex(
                b"1234567890123456789012345678901234567890\
                1234567890123456789012345678901234567890"
            ),
            "e33b4ddc9c38f2199c3e7b164fcc0536"
        );
    }

    #[test]
    fn hashes_input_that_does_not_stop_at_a_block() {
        // A message of exactly one block still needs a block of padding, and
        // one of 56 bytes leaves no room for the length in its own block.
        assert_eq!(md4_hex(&[0u8; 64]).len(), 32);
        assert_eq!(md4_hex(&[0u8; 56]).len(), 32);
        // A block of zeroes is what the last, short block of a target file is
        // padded with, so it has to hash like any other input.
        assert_ne!(md4(&[0u8; 64]), md4(&[0u8; 56]));
        // Padding is what changes between these two, not the content.
        assert_ne!(md4(&[0u8; 64]), md4(&[0u8; 65]));
    }

    #[test]
    fn the_pieces_md4_input_arrives_in_do_not_matter() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        for size in [1, 7, 55, 56, 63, 64, 65, 128] {
            let mut chunked = Md4::new();
            for piece in data.chunks(size) {
                chunked.update(piece);
            }
            assert_eq!(chunked.finish(), md4(&data), "in pieces of {size}");
        }
    }
}
