//! The zsync file itself: the text header a check reads, and the table of
//! block checksums applying a delta reads.
//!
//! A zsync file starts with a short text header, then a blank line, then the
//! block checksums. The header names the length of the complete file and its
//! SHA-1, so a single ranged request for the first few kilobytes decides
//! whether an update exists; [`parse_header`] is what a check uses and it
//! never downloads a payload.
//!
//! [`parse_control`] reads the same file to the end: two checksums per block
//! of the complete file, a weak rolling one and a strong one, both truncated
//! to the lengths the `Hash-Lengths` line gives.
//!
//! The lengths, ranges and byte order follow zsync 0.6.2, `libzsync/zsync.c`.

use crate::download;
use crate::error::{Error, Result};

use super::rsum::Rsum;

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
    /// The size of one block, a power of two. Absent in a header that a
    /// check can still use, required to read the block table.
    pub blocksize: Option<u64>,
    /// How long the two checksums of a block are, and how many blocks in a
    /// row have to match. Falls back to what zsync assumed before the field
    /// existed.
    pub hash_lengths: HashLengths,
}

/// The `Hash-Lengths: 2,2,5` line: two consecutive blocks must match, the
/// rolling checksum is kept to two bytes and the strong one to five.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashLengths {
    /// How many consecutive blocks must match before a hit counts, 1 or 2.
    pub seq_matches: u8,
    /// Bytes of the rolling checksum the table holds, 1 to 4.
    pub rsum_bytes: u8,
    /// Bytes of the strong checksum the table holds, 3 to 16.
    pub checksum_bytes: u8,
}

impl HashLengths {
    /// What a header without a `Hash-Lengths` line means: the untruncated
    /// checksums that zsync used before the field existed.
    pub const DEFAULT: Self = Self { seq_matches: 1, rsum_bytes: 4, checksum_bytes: 16 };

    /// The bytes one block occupies in the table.
    pub fn entry_size(self) -> usize {
        self.rsum_bytes as usize + self.checksum_bytes as usize
    }

    /// The part of a computed [`Rsum`] that a stored one can be compared
    /// against: the table only published `rsum_bytes` of it.
    ///
    /// zsync masks the halves separately, `librcksum/state.c:48`: `b` is
    /// always compared whole, `a` through a mask of nothing, one byte or two
    /// as `rsum_bytes` is below three, three, or four. For every length a
    /// real zsync writes that comes to the same thing as keeping the low
    /// `rsum_bytes` bytes. At one byte it does not: zsync then compares a
    /// whole `b` against a stored value that only ever held eight bits of
    /// it, and this mask keeps that quirk rather than inventing a friendlier
    /// rule the reference would disagree with.
    pub fn rsum_mask(self) -> u32 {
        let a_mask: u32 = match self.rsum_bytes {
            0..=2 => 0,
            3 => 0xff,
            _ => 0xffff,
        };
        (a_mask << 16) | 0xffff
    }

    /// A computed checksum reduced to what the table holds, ready to be
    /// compared with [`BlockSum::rsum`].
    pub fn truncate(self, rsum: Rsum) -> u32 {
        rsum.value() & self.rsum_mask()
    }

    /// The ranges are zsync's own: anything outside them means the file was
    /// not written by a zsync that this one can talk to.
    pub(super) fn parse(value: &str) -> std::result::Result<Self, String> {
        let complaint = || format!("{value:?} is not a usable Hash-Lengths line");
        let mut fields = value.split(',').map(str::trim);
        let seq = fields.next().and_then(|field| field.parse::<u8>().ok());
        let weak = fields.next().and_then(|field| field.parse::<u8>().ok());
        let strong = fields.next().and_then(|field| field.parse::<u8>().ok());

        let (Some(seq_matches), Some(rsum_bytes), Some(checksum_bytes), None) =
            (seq, weak, strong, fields.next())
        else {
            return Err(complaint());
        };
        if !(1..=2).contains(&seq_matches)
            || !(1..=4).contains(&rsum_bytes)
            || !(3..=16).contains(&checksum_bytes)
        {
            return Err(complaint());
        }
        Ok(Self { seq_matches, rsum_bytes, checksum_bytes })
    }
}

/// The two checksums the table holds for one block of the complete file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSum {
    /// The rolling checksum, in the low `rsum_bytes` bytes of the value: the
    /// table stores the last bytes of the big endian full checksum, so a
    /// scanner compares its own checksum truncated the same way. The bytes
    /// above that are zero.
    pub rsum: u32,
    /// The strong checksum, of which only the first `checksum_bytes` bytes
    /// were published. The rest is zero, [`ControlFile::checksum`] hands out
    /// the part that means anything.
    pub checksum: [u8; 16],
}

/// A whole zsync file: the header, and one [`BlockSum`] per block of the
/// complete file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFile {
    pub header: Header,
    /// The size of one block. The last block of the file is shorter than
    /// this, zsync checksums it zero padded to the full block.
    pub blocksize: u64,
    pub hash_lengths: HashLengths,
    /// One entry per block, in order.
    pub blocks: Vec<BlockSum>,
}

impl ControlFile {
    /// The log2 of the block size, which is what [`Rsum::roll`] shifts by.
    pub fn blockshift(&self) -> u32 {
        self.blocksize.trailing_zeros()
    }

    /// The part of a block's strong checksum that was actually published.
    pub fn checksum(&self, block: usize) -> Option<&[u8]> {
        self.blocks.get(block).map(|sum| &sum.checksum[..self.hash_lengths.checksum_bytes as usize])
    }
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
    let (end, _) = terminator(bytes).ok_or_else(|| {
        "the header is not terminated by a blank line, it did not fit in one request".to_string()
    })?;
    let text = String::from_utf8_lossy(&bytes[..end]);

    let mut header = Header {
        filename: None,
        length: 0,
        sha1: None,
        url: None,
        mtime: None,
        blocksize: None,
        hash_lengths: HashLengths::DEFAULT,
    };
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
            "blocksize" => {
                let size =
                    value.parse::<u64>().map_err(|_| format!("{value:?} is not a block size"))?;
                // zsync only ever writes powers of two and its own reader
                // refuses anything else.
                if size == 0 || !size.is_power_of_two() {
                    return Err(format!("{size} is not a usable block size"));
                }
                header.blocksize = Some(size);
            }
            "hash-lengths" => header.hash_lengths = HashLengths::parse(&value)?,
            _ => {}
        }
    }

    header.length = length.ok_or_else(|| "the header carries no Length".to_string())?;
    Ok(header)
}

/// Where the blank line that ends the header starts, and how many bytes it
/// takes. The block checksums begin right after it, and they are binary, so
/// a stray `\n\n` inside them must not be mistaken for the end of a header
/// that ended in `\r\n\r\n` earlier: whichever form comes first wins.
fn terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|pair| pair == b"\n\n").map(|at| (at, 2));
    let crlf = bytes.windows(4).position(|quad| quad == b"\r\n\r\n").map(|at| (at, 4));
    match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(if lf.0 <= crlf.0 { lf } else { crlf }),
        (found, other) => found.or(other),
    }
}

/// Parses a whole zsync file: the header, then one entry per block of the
/// complete file. The caller has to have the whole file, a short one is an
/// error rather than a shorter table.
pub fn parse_control(bytes: &[u8]) -> std::result::Result<ControlFile, String> {
    let header = parse_header(bytes)?;
    let (end, skip) = terminator(bytes).ok_or_else(|| {
        "the header is not terminated by a blank line, it did not fit in one request".to_string()
    })?;

    let blocksize =
        header.blocksize.ok_or_else(|| "the header carries no Blocksize".to_string())?;
    if header.length == 0 {
        return Err("the header says the complete file is empty".to_string());
    }

    let hash_lengths = header.hash_lengths;
    let entry_size = hash_lengths.entry_size();
    let count = header.length.div_ceil(blocksize);
    // A wrong length and a tiny block size together ask for more entries than
    // could ever be there. The size of the table settles it before anything
    // is allocated.
    let needed = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(entry_size))
        .ok_or_else(|| {
            format!("a file of {} bytes in blocks of {blocksize} would need more block checksums than fit in memory", header.length)
        })?;

    let table = &bytes[end + skip..];
    if table.len() < needed {
        return Err(format!(
            "the block table is short: {count} blocks need {needed} bytes, the file has {}",
            table.len()
        ));
    }

    let mut blocks = Vec::with_capacity(count as usize);
    for entry in table[..needed].chunks_exact(entry_size) {
        let (weak, strong) = entry.split_at(hash_lengths.rsum_bytes as usize);
        // The table holds the last bytes of the big endian rolling checksum,
        // which is what zsync reads into the tail of its own four byte value.
        let mut rsum = [0u8; 4];
        rsum[4 - weak.len()..].copy_from_slice(weak);
        let mut checksum = [0u8; 16];
        checksum[..strong.len()].copy_from_slice(strong);
        blocks.push(BlockSum { rsum: u32::from_be_bytes(rsum), checksum });
    }

    Ok(ControlFile { header, blocksize, hash_lengths, blocks })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

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
        assert_eq!(header.blocksize, Some(4096));
        assert_eq!(
            header.hash_lengths,
            HashLengths { seq_matches: 2, rsum_bytes: 2, checksum_bytes: 4 }
        );
    }

    #[test]
    fn falls_back_to_the_lengths_zsync_used_before_the_field_existed() {
        let file = b"zsync: 0.6.2\nBlocksize: 2048\nLength: 10\n\n";
        let header = parse_header(file).unwrap();
        assert_eq!(header.hash_lengths, HashLengths::DEFAULT);
        assert_eq!(header.hash_lengths.entry_size(), 20);
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

    /// A zsync file with a header the test controls and a table it wrote.
    pub(crate) fn control_bytes(
        length: u64,
        blocksize: u64,
        lengths: Option<&str>,
        table: &[u8],
    ) -> Vec<u8> {
        let mut out = format!("zsync: 0.6.2\nBlocksize: {blocksize}\nLength: {length}\n");
        if let Some(lengths) = lengths {
            out.push_str(&format!("Hash-Lengths: {lengths}\n"));
        }
        out.push('\n');
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(table);
        bytes
    }

    /// Three blocks with 2 byte rolling and 4 byte strong checksums, the
    /// entries numbered so a swapped or shifted read is visible.
    fn three_blocks() -> Vec<u8> {
        let mut table = Vec::new();
        for block in 0..3u8 {
            table.extend_from_slice(&[0x10 + block, 0x20 + block]);
            table.extend_from_slice(&[0xa0 + block, 0xb0 + block, 0xc0 + block, 0xd0 + block]);
        }
        table
    }

    #[test]
    fn reads_a_table_of_known_contents() {
        // 10000 bytes in blocks of 4096 is three blocks, the last one short.
        let file = control_bytes(10_000, 4096, Some("2,2,4"), &three_blocks());
        let control = parse_control(&file).unwrap();

        assert_eq!(control.blocksize, 4096);
        assert_eq!(control.hash_lengths.seq_matches, 2);
        assert_eq!(control.blocks.len(), 3);
        assert_eq!(control.blocks[0].rsum, 0x0000_1020);
        assert_eq!(control.blocks[2].rsum, 0x0000_1222);
        assert_eq!(control.checksum(0).unwrap(), &[0xa0, 0xb0, 0xc0, 0xd0]);
        assert_eq!(control.checksum(2).unwrap(), &[0xa2, 0xb2, 0xc2, 0xd2]);
        assert_eq!(control.checksum(3), None);
        // Only the published bytes of the strong checksum are kept.
        assert_eq!(control.blocks[0].checksum[4..], [0u8; 12]);
    }

    #[test]
    fn a_block_is_exactly_one_entry_long() {
        // One byte over two blocks is three blocks, not two.
        let file = control_bytes(8193, 4096, Some("2,2,4"), &three_blocks());
        assert_eq!(parse_control(&file).unwrap().blocks.len(), 3);
        // And a length that divides evenly is not rounded up.
        let file = control_bytes(8192, 4096, Some("2,2,4"), &three_blocks()[..12]);
        assert_eq!(parse_control(&file).unwrap().blocks.len(), 2);
    }

    #[test]
    fn the_rolling_checksum_keeps_its_place_in_the_full_value() {
        // Four bytes: the whole checksum, byte order as written.
        let file = control_bytes(1, 4096, Some("1,4,3"), &[0x12, 0x34, 0x56, 0x78, 1, 2, 3]);
        assert_eq!(parse_control(&file).unwrap().blocks[0].rsum, 0x1234_5678);
        // Three: the low three bytes, the top one zero.
        let file = control_bytes(1, 4096, Some("1,3,3"), &[0x34, 0x56, 0x78, 1, 2, 3]);
        assert_eq!(parse_control(&file).unwrap().blocks[0].rsum, 0x0034_5678);
        // One: the lowest byte only.
        let file = control_bytes(1, 4096, Some("1,1,3"), &[0x78, 1, 2, 3]);
        assert_eq!(parse_control(&file).unwrap().blocks[0].rsum, 0x0000_0078);
    }

    #[test]
    fn reads_a_table_written_with_crlf_line_endings() {
        // The table itself carries a blank line, which must not be taken for
        // the end of the header.
        let mut file = b"zsync: 0.6.2\r\nBlocksize: 4096\r\nLength: 4096\r\n\
            Hash-Lengths: 1,2,3\r\n\r\n"
            .to_vec();
        file.extend_from_slice(&[0x0a, 0x0a, 0xff, 0xfe, 0xfd]);
        let control = parse_control(&file).unwrap();
        assert_eq!(control.blocks.len(), 1);
        assert_eq!(control.blocks[0].rsum, 0x0000_0a0a);
        assert_eq!(control.checksum(0).unwrap(), &[0xff, 0xfe, 0xfd]);
    }

    #[test]
    fn ignores_anything_written_past_the_last_block() {
        let mut file = control_bytes(4096, 4096, Some("1,2,3"), &[1, 2, 3, 4, 5]);
        file.extend_from_slice(b"trailing");
        assert_eq!(parse_control(&file).unwrap().blocks.len(), 1);
    }

    #[test]
    fn rejects_a_table_that_does_not_hold_every_block() {
        // Three blocks of six bytes each, two of them present.
        let file = control_bytes(10_000, 4096, Some("2,2,4"), &three_blocks()[..12]);
        let reason = parse_control(&file).unwrap_err();
        assert!(reason.contains("short"), "{reason}");
        // Not even one whole entry.
        let file = control_bytes(4096, 4096, Some("2,2,4"), &[1, 2, 3]);
        assert!(parse_control(&file).is_err());
        // No table at all.
        let file = control_bytes(4096, 4096, Some("2,2,4"), &[]);
        assert!(parse_control(&file).is_err());
    }

    #[test]
    fn rejects_a_header_the_table_cannot_be_read_with() {
        // No Blocksize: the number of blocks is unknown.
        let file = b"zsync: 0.6.2\nLength: 4096\n\n\x01\x02";
        assert!(parse_control(file).is_err());
        // A complete file of no length has no blocks to describe.
        let file = control_bytes(0, 4096, Some("2,2,4"), &three_blocks());
        assert!(parse_control(&file).is_err());
        // A block size that is not a power of two is refused by zsync itself.
        assert!(parse_header(b"zsync: 0.6.2\nBlocksize: 3000\nLength: 10\n\n").is_err());
        assert!(parse_header(b"zsync: 0.6.2\nBlocksize: 0\nLength: 10\n\n").is_err());
        assert!(parse_header(b"zsync: 0.6.2\nBlocksize: huge\nLength: 10\n\n").is_err());
    }

    #[test]
    fn rejects_hash_lengths_outside_the_ranges_zsync_allows() {
        for line in ["0,2,4", "3,2,4", "2,0,4", "2,5,4", "2,2,2", "2,2,17"] {
            let file = control_bytes(4096, 4096, Some(line), &[0; 32]);
            assert!(parse_control(&file).is_err(), "{line} was accepted");
        }
        for line in ["2,2", "2,2,4,1", "", "a,b,c", "2, ,4", "-1,2,4"] {
            let file = control_bytes(4096, 4096, Some(line), &[0; 32]);
            assert!(parse_control(&file).is_err(), "{line:?} was accepted");
        }
        // The ends of the ranges are usable.
        for line in ["1,1,3", "2,4,16"] {
            let file = control_bytes(4096, 4096, Some(line), &[0; 32]);
            assert!(parse_control(&file).is_ok(), "{line} was refused");
        }
    }

    #[test]
    fn refuses_a_table_larger_than_the_file_it_came_in() {
        // A length near the top of u64 with the smallest block size asks for
        // more entries than exist, and must not be allocated for.
        let file = control_bytes(u64::MAX, 1, Some("1,1,3"), &[0; 32]);
        assert!(parse_control(&file).is_err());
        let file = control_bytes(u64::MAX / 2, 1, Some("1,1,3"), &[0; 32]);
        assert!(parse_control(&file).is_err());
    }

    #[test]
    fn survives_input_no_zsync_ever_wrote() {
        for file in [
            &b""[..],
            &b"\n"[..],
            &b"\n\n"[..],
            &b"\0"[..],
            &b"zsync: 0.6.2\n"[..],
            &b"zsync: 0.6.2\n\n"[..],
            &b"<html>\n\n</html>"[..],
            &[0xff; 64][..],
        ] {
            assert!(parse_control(file).is_err(), "{file:?} was accepted");
        }
    }

    #[test]
    fn reads_a_table_the_size_a_large_appimage_needs() {
        // What a 380 MB AppImage looks like in a zsync written with the
        // lengths Krita's build uses: 94217 blocks of seven bytes each. Built
        // here rather than kept as a fixture, the bytes are what matter.
        let blocks = 94_217u32;
        let mut table = Vec::with_capacity(blocks as usize * 7);
        for block in 0..blocks {
            table.extend_from_slice(&(block as u16).to_be_bytes());
            table.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, block as u8]);
        }

        let control =
            parse_control(&control_bytes(385_911_288, 4096, Some("2,2,5"), &table)).unwrap();
        let last = blocks as usize - 1;
        assert_eq!(control.blocks.len(), blocks as usize);
        assert_eq!(control.hash_lengths.entry_size(), 7);
        assert_eq!(control.blocks[last].rsum, u32::from(last as u16));
        assert_eq!(control.checksum(last).unwrap(), &[0xaa, 0xbb, 0xcc, 0xdd, last as u8]);
    }
}
