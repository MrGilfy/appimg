//! The zsync format: reading a control file, and finding what a local file
//! already holds of the complete file it describes.
//!
//! [`control`] parses the file a server publishes, [`rsum`] and [`md4`] are
//! the two checksums it holds per block, [`scan`] slides one over a local
//! file to find the blocks that need no fetching, and [`fetch`] asks the
//! server for the rest. Everything follows zsync 0.6.2; each module names
//! the lines it was derived from.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::{Error, Result};

pub mod control;
pub mod fetch;
pub mod md4;
pub mod rsum;
pub mod scan;

pub use control::{
    fetch_header, parse_control, parse_header, BlockSum, ControlFile, HashLengths, Header,
};
pub use fetch::{fetch_missing, missing_ranges, FetchReport};
pub use md4::{md4, Md4};
pub use rsum::Rsum;
pub use scan::{scan, scan_file, SourceMap};

/// The SHA-1 of a file, lowercase hex. Read in chunks, an AppImage is large.
pub fn sha1_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut digest = Sha1::new();
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => digest.update(&buffer[..read]),
            Err(e) => return Err(Error::io(path, e)),
        }
    }
    Ok(digest.finish_hex())
}

/// SHA-1 as in RFC 3174. zsync writes it into every header, so a check that
/// wants to compare more than the length has to compute it. It is used here
/// to tell two builds apart, never to prove anything about a file's origin.
struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    used: usize,
    length: u64,
}

impl Sha1 {
    fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0],
            block: [0u8; 64],
            used: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
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

    fn finish_hex(mut self) -> String {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        // The length has to land in the last eight bytes of a block.
        while self.used != 56 {
            self.update(&[0]);
        }
        let mut last = self.block;
        last[56..].copy_from_slice(&bits.to_be_bytes());
        self.compress(&last);

        let mut out = String::with_capacity(40);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut words = [0u32; 80];
        for (word, chunk) in words.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*chunk);
        }
        for i in 16..80 {
            words[i] = (words[i - 3] ^ words[i - 8] ^ words[i - 14] ^ words[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (i, word) in words.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha1_hex(data: &[u8]) -> String {
        let mut digest = Sha1::new();
        digest.update(data);
        digest.finish_hex()
    }

    #[test]
    fn hashes_the_rfc_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // Long enough to cross several blocks, and to exercise the padding
        // that has to spill into a block of its own.
        assert_eq!(sha1_hex(&vec![b'a'; 1_000_000]), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
        // A message of exactly one block still needs a block of padding.
        assert_eq!(sha1_hex(&[0u8; 64]).len(), 40);
    }

    #[test]
    fn the_pieces_data_arrives_in_do_not_matter() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 253) as u8).collect();
        let mut chunked = Sha1::new();
        for piece in data.chunks(7) {
            chunked.update(piece);
        }
        assert_eq!(chunked.finish_hex(), sha1_hex(&data));
    }
}
