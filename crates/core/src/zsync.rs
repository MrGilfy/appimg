//! Just enough of the zsync format to answer one question: is the local file
//! still the one the server offers?
//!
//! A zsync file starts with a short text header, then a blank line, then the
//! block checksums. The header names the length of the complete file and its
//! SHA-1, so a single ranged request for the first few kilobytes decides
//! whether an update exists. Applying the delta afterwards is still
//! `appimageupdatetool`'s job, this module never downloads a payload.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::download;
use crate::error::{Error, Result};

/// How much of the zsync file is asked for. Real headers are a few hundred
/// bytes; anything past this is block checksums, which a check never needs.
const HEADER_REQUEST: usize = 8 * 1024;

/// The text header of a zsync file. Only the fields a check uses are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The name of the complete file, which usually carries its version.
    pub filename: Option<String>,
    /// The length of the complete file in bytes.
    pub length: u64,
    /// The SHA-1 of the complete file, lowercase hex.
    pub sha1: Option<String>,
    /// The URL the payload would be fetched from, as written in the header.
    pub url: Option<String>,
    pub mtime: Option<String>,
}

/// Reads the header of a zsync file with a single ranged request. Servers
/// that ignore the range send more, so the reader is capped either way.
pub fn fetch_header(url: &str) -> Result<Header> {
    let head = download::head_bytes(url, HEADER_REQUEST)?;
    parse_header(&head).map_err(|reason| Error::Zsync { url: url.to_string(), reason })
}

/// Parses the text part of a zsync file. The error is a plain reason, the
/// caller knows which URL it came from.
pub fn parse_header(bytes: &[u8]) -> std::result::Result<Header, String> {
    // The header is ASCII, the block checksums that follow are not, so only
    // the part before the blank line is ever looked at.
    let end = terminator(bytes).ok_or_else(|| {
        "the header is not terminated by a blank line, it did not fit in one request".to_string()
    })?;
    let text = String::from_utf8_lossy(&bytes[..end]);

    let mut header = Header { filename: None, length: 0, sha1: None, url: None, mtime: None };
    let mut length = None;
    let mut first = true;

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("{line:?} is not a header line"));
        };
        let value = value.trim().to_string();
        let key = key.trim().to_ascii_lowercase();

        if first {
            if key != "zsync" {
                return Err("the file does not start with a zsync version".to_string());
            }
            first = false;
        }
        match key.as_str() {
            "filename" => header.filename = Some(value),
            "mtime" => header.mtime = Some(value),
            "sha-1" => header.sha1 = Some(value.to_ascii_lowercase()),
            // A URL line may itself contain a colon, which `split_once`
            // already handled by splitting on the first one.
            "url" => header.url = Some(value),
            "length" => {
                length =
                    Some(value.parse::<u64>().map_err(|_| format!("{value:?} is not a length"))?);
            }
            _ => {}
        }
    }

    header.length = length.ok_or_else(|| "the header carries no Length".to_string())?;
    Ok(header)
}

/// The offset of the blank line that ends the header.
fn terminator(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .or_else(|| bytes.windows(4).position(|quad| quad == b"\r\n\r\n"))
}

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

    #[test]
    fn reads_the_fields_a_check_needs() {
        let file = b"zsync: 0.6.2\nFilename: App-2.0.0-x86_64.AppImage\n\
            MTime: Sat, 01 Aug 2026 10:00:00 +0000\nBlocksize: 4096\nLength: 91234567\n\
            Hash-Lengths: 2,2,4\nURL: App-2.0.0-x86_64.AppImage\n\
            SHA-1: A9993E364706816ABA3E25717850C26C9CD0D89D\n\n\xff\x00binary";

        let header = parse_header(file).unwrap();
        assert_eq!(header.filename.as_deref(), Some("App-2.0.0-x86_64.AppImage"));
        assert_eq!(header.length, 91_234_567);
        assert_eq!(header.sha1.as_deref(), Some("a9993e364706816aba3e25717850c26c9cd0d89d"));
        assert_eq!(header.url.as_deref(), Some("App-2.0.0-x86_64.AppImage"));
    }

    #[test]
    fn accepts_crlf_and_absolute_urls() {
        let file =
            b"zsync: 0.6.2\r\nLength: 10\r\nURL: https://example.com/App.AppImage\r\n\r\n\x00";
        let header = parse_header(file).unwrap();
        assert_eq!(header.length, 10);
        assert_eq!(header.url.as_deref(), Some("https://example.com/App.AppImage"));
    }

    #[test]
    fn rejects_anything_that_is_not_a_zsync_header() {
        // An error page, which is what a wrong URL usually returns.
        assert!(parse_header(b"<html>\n<body>404</body>\n\n</html>").is_err());
        // No blank line: the header did not fit into the request.
        assert!(parse_header(b"zsync: 0.6.2\nLength: 10\n").is_err());
        // No length: nothing to compare against.
        assert!(parse_header(b"zsync: 0.6.2\nFilename: App\n\nrest").is_err());
        assert!(parse_header(b"zsync: 0.6.2\nLength: huge\n\nrest").is_err());
    }
}
