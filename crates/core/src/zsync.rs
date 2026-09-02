//! The zsync format: the text header a check needs, and the table of block
//! checksums applying a delta needs.
//!
//! A zsync file starts with a short text header, then a blank line, then the
//! block checksums. The header names the length of the complete file and its
//! SHA-1, so a single ranged request for the first few kilobytes decides
//! whether an update exists; [`parse_header`] is what a check uses and it
//! never downloads a payload.
//!
//! [`parse_control`] reads the same file to the end: two checksums per block
//! of the complete file, a weak rolling one and a strong one, both truncated
//! to the lengths the `Hash-Lengths` line gives. Nothing in here computes a
//! checksum of the local file yet, it only reads what the server published.
//!
//! The lengths, ranges and byte order follow zsync 0.6.2, `libzsync/zsync.c`.

use std::fs::File;
use std::io::{self, Read};
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
    fn parse(value: &str) -> std::result::Result<Self, String> {
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

/// How much of the source file the scanner holds at a time. The buffer is
/// this plus one window of overlap and nothing else grows with the length of
/// the file, so a 380 MB seed is read in pieces of this size.
const SCAN_CHUNK: usize = 64 * 1024;

/// The largest block size the scanner will scan with. A header may name any
/// power of two, and a window of a gigabyte is a header lying about what it
/// describes rather than a file worth reading.
const MAX_BLOCKSIZE: u64 = 64 * 1024 * 1024;

/// A block that has not been found in the local file.
const NOT_FOUND: u64 = u64::MAX;

/// Which blocks of the complete file a local file already holds, and at what
/// offset each was found. Offsets are not multiples of the block size: the
/// whole point of the scan is to find the target's blocks wherever they
/// drifted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMap {
    offsets: Vec<u64>,
    matched: usize,
}

impl SourceMap {
    fn new(blocks: usize) -> Self {
        Self { offsets: vec![NOT_FOUND; blocks], matched: 0 }
    }

    /// How many blocks the complete file has.
    pub fn blocks(&self) -> usize {
        self.offsets.len()
    }

    /// How many of them the local file turned out to hold.
    pub fn matched(&self) -> usize {
        self.matched
    }

    /// Whether the local file holds every block, which means nothing has to
    /// be fetched at all.
    pub fn is_complete(&self) -> bool {
        self.matched == self.offsets.len()
    }

    /// Where a block was found in the local file.
    pub fn offset_of(&self, block: usize) -> Option<u64> {
        match self.offsets.get(block) {
            Some(&NOT_FOUND) | None => None,
            Some(&offset) => Some(offset),
        }
    }

    /// The first offset wins. A block found twice is the same bytes either
    /// way, and reading the earlier copy is the friendlier seek.
    fn record(&mut self, block: usize, offset: u64) {
        if self.offsets[block] == NOT_FOUND {
            self.offsets[block] = offset;
            self.matched += 1;
        }
    }
}

/// The blocks of the target file, in buckets by rolling checksum, so a
/// position in the local file can be tested with one lookup instead of a
/// walk over every block.
///
/// The hash and its size are zsync's, `librcksum/hash.c:57` onwards: the key
/// mixes the low half of one block's checksum with the low half of the next
/// one when two blocks have to match in sequence, and with the masked high
/// half when only one does.
struct BlockIndex {
    mask: u32,
    /// First block in each bucket, or `NO_BLOCK`.
    heads: Vec<u32>,
    /// The next block in the same bucket, or `NO_BLOCK`.
    next: Vec<u32>,
}

const NO_BLOCK: u32 = u32::MAX;

impl BlockIndex {
    fn build(control: &ControlFile) -> Self {
        let blocks = control.blocks.len();

        // zsync tries 2^17 buckets and steps down while that is more than
        // twice what the blocks need, never below 2^5.
        let mut bits = 16u32;
        while (1u64 << bits) > blocks as u64 && bits > 4 {
            bits -= 1;
        }
        let size = 1usize << (bits + 1);

        let mut index = Self {
            mask: (size - 1) as u32,
            heads: vec![NO_BLOCK; size],
            next: vec![NO_BLOCK; blocks],
        };

        // The last block has no block after it to pair with, so with
        // sequential matching it is not reachable by lookup at all; it is
        // found by continuing the run that reaches it, which is how the
        // reference ends up behaving too.
        let indexable =
            if control.hash_lengths.seq_matches > 1 { blocks.saturating_sub(1) } else { blocks };

        // Backwards, so each bucket ends up holding its blocks in order.
        for block in (0..indexable).rev() {
            let bucket = (index.key_of_block(control, block) & index.mask) as usize;
            index.next[block] = index.heads[bucket];
            index.heads[bucket] = block as u32;
        }
        index
    }

    /// The bucket a block belongs in, from what the table holds for it.
    fn key_of_block(&self, control: &ControlFile, block: usize) -> u32 {
        let stored = control.blocks[block].rsum;
        let second = if control.hash_lengths.seq_matches > 1 {
            control.blocks[block + 1].rsum & 0xffff
        } else {
            (stored >> 16) & (control.hash_lengths.rsum_mask() >> 16)
        };
        (stored & 0xffff) ^ (second << 3)
    }

    /// The bucket a position in the local file would be in, from the rolling
    /// checksums of the window there and of the window one block on.
    fn key_of_window(&self, control: &ControlFile, first: Rsum, second: Rsum) -> usize {
        let paired = if control.hash_lengths.seq_matches > 1 {
            u32::from(second.b)
        } else {
            u32::from(first.a) & (control.hash_lengths.rsum_mask() >> 16)
        };
        ((u32::from(first.b) ^ (paired << 3)) & self.mask) as usize
    }
}

/// The local file, read in pieces, with enough of the previous piece kept to
/// carry a window across the seam.
struct Source<R> {
    reader: R,
    buffer: Vec<u8>,
    /// Bytes of `buffer` that hold data, including the padding at EOF.
    filled: usize,
    /// The offset in the file that `buffer[0]` holds.
    base: u64,
    /// One whole window: block size times the blocks that must match in
    /// sequence.
    context: usize,
    /// Whether the zeroes zsync pads a source with at EOF are in place.
    padded: bool,
}

impl<R: Read> Source<R> {
    fn new(reader: R, context: usize) -> Self {
        // Room for a chunk of file plus the padding, and never so small that
        // a window and its padding cannot both be held.
        let chunk = SCAN_CHUNK.max(context * 2);
        Self {
            reader,
            buffer: vec![0u8; chunk + context],
            filled: 0,
            base: 0,
            context,
            padded: false,
        }
    }

    /// How much of the buffer may hold file data; the rest is kept free for
    /// the padding.
    fn limit(&self) -> usize {
        self.buffer.len() - self.context
    }

    /// Makes the window at `at` readable, reading more of the file if it has
    /// to. False means the scan has reached the end: `at` is past the last
    /// byte of the file.
    ///
    /// The caller never asks for a position more than one window beyond the
    /// last one it was given, so the bytes it wants are either in the buffer
    /// already or come out of the next read.
    fn ready(&mut self, at: u64) -> io::Result<bool> {
        loop {
            let start = (at - self.base) as usize;
            // zsync stops when the window plus its context reaches the end
            // of what it has, `librcksum/rsum.c:326`.
            if start + self.context < self.filled {
                return Ok(true);
            }
            if self.padded {
                return Ok(false);
            }

            // Drop what the scan has gone past, keep the rest at the front.
            if start > 0 {
                self.buffer.copy_within(start..self.filled, 0);
                self.filled -= start;
                self.base = at;
            }

            let limit = self.limit();
            match self.reader.read(&mut self.buffer[self.filled..limit])? {
                0 => {
                    // EOF: zsync pads the source with a window of zeroes so
                    // that the last, short block of the target can still
                    // match, `librcksum/rsum.c:466`.
                    let end = self.filled + self.context;
                    self.buffer[self.filled..end].fill(0);
                    self.filled = end;
                    self.padded = true;
                }
                read => self.filled += read,
            }
        }
    }

    fn data(&self) -> &[u8] {
        &self.buffer[..self.filled]
    }

    fn index_of(&self, at: u64) -> usize {
        (at - self.base) as usize
    }
}

/// Finds the blocks of the complete file that a local file already holds.
///
/// Reads the file in pieces, so the memory this needs is the block table and
/// one buffer, whatever the size of the file.
pub fn scan_file(control: &ControlFile, path: &Path) -> Result<SourceMap> {
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    scan(control, file).map_err(|e| Error::io(path, e))
}

/// The same, over anything that reads bytes.
///
/// The window slides one byte at a time, as in `librcksum/rsum.c:301`
/// onwards: a lookup by rolling checksum, then MD4 to settle it, then a jump
/// of a whole block when the bytes turn out to be a block of the target.
pub fn scan<R: Read>(control: &ControlFile, reader: R) -> io::Result<SourceMap> {
    let blocks = control.blocks.len();
    let mut map = SourceMap::new(blocks);
    if blocks == 0 {
        return Ok(map);
    }
    if control.blocksize > MAX_BLOCKSIZE || blocks > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "a zsync file of {blocks} blocks of {} bytes is too large to scan with",
                control.blocksize
            ),
        ));
    }

    let blocksize = control.blocksize as usize;
    let sequential = usize::from(control.hash_lengths.seq_matches);
    let context = blocksize * sequential;
    let shift = control.blockshift();

    let index = BlockIndex::build(control);
    let mut source = Source::new(reader, context);

    let mut at = 0u64;
    // The checksums of the window at the current position and, when blocks
    // must match in sequence, of the window one block on. `None` after a
    // jump, when they have to be computed afresh.
    let mut checksums: Option<(Rsum, Rsum)> = None;
    // The block that would continue the run of matches we are in.
    let mut run: Option<usize> = None;

    while source.ready(at)? {
        let start = source.index_of(at);
        let data = source.data();
        let window = &data[start..start + blocksize];

        let (first, second) = match checksums {
            Some(pair) => pair,
            None => (
                Rsum::of(window),
                if sequential > 1 {
                    Rsum::of(&data[start + blocksize..start + 2 * blocksize])
                } else {
                    Rsum::default()
                },
            ),
        };

        let mut matched = 0usize;

        // A run of matches carries on if the next block of the target is
        // here: one block has to check out, not a pair of them.
        if let Some(block) = run {
            if weak_hit(control, block, first) && strong_hit(control, block, &md4(window)) {
                map.record(block, at);
                matched = 1;
                run = if block + 1 < blocks { Some(block + 1) } else { None };
            }
        }

        if matched == 0 {
            let bucket = index.key_of_window(control, first, second);
            let mut digests: (Option<[u8; 16]>, Option<[u8; 16]>) = (None, None);
            let mut candidate = index.heads[bucket];

            while candidate != NO_BLOCK {
                let block = candidate as usize;
                candidate = index.next[block];

                if !weak_hit(control, block, first) {
                    continue;
                }
                if sequential > 1 && !weak_hit(control, block + 1, second) {
                    continue;
                }

                let first_digest = *digests.0.get_or_insert_with(|| md4(window));
                if !strong_hit(control, block, &first_digest) {
                    continue;
                }
                if sequential > 1 {
                    let next = &data[start + blocksize..start + 2 * blocksize];
                    let second_digest = *digests.1.get_or_insert_with(|| md4(next));
                    if !strong_hit(control, block + 1, &second_digest) {
                        continue;
                    }
                }

                map.record(block, at);
                if sequential > 1 {
                    map.record(block + 1, at + blocksize as u64);
                }
                matched = sequential;
                run = if block + sequential < blocks { Some(block + sequential) } else { None };
            }
        }

        if matched > 0 {
            // The target's blocks are a block apart, so a hit at this
            // position makes a hit one byte on unlikely enough to skip.
            at += (blocksize * matched) as u64;
            checksums = None;
            continue;
        }

        let leaving = data[start];
        let entering = data[start + blocksize];
        let mut first = first;
        first.roll(leaving, entering, shift);
        let second = if sequential > 1 {
            let mut second = second;
            second.roll(entering, data[start + 2 * blocksize], shift);
            second
        } else {
            second
        };

        checksums = Some((first, second));
        // A run only ever continues at the next block boundary.
        run = None;
        at += 1;
    }

    Ok(map)
}

/// Whether the rolling checksum of a window is the one the table holds for a
/// block, compared through the truncation the table was written with.
fn weak_hit(control: &ControlFile, block: usize, rsum: Rsum) -> bool {
    control.hash_lengths.truncate(rsum) == control.blocks[block].rsum
}

/// Whether an MD4 is the one the table holds for a block, over the bytes of
/// it that were published.
fn strong_hit(control: &ControlFile, block: usize, digest: &[u8; 16]) -> bool {
    control.checksum(block) == Some(&digest[..control.hash_lengths.checksum_bytes as usize])
}

/// The weak, rolling checksum of one window of a file: two sixteen bit
/// halves, `a` the sum of the bytes and `b` the sum weighted by how far each
/// byte is from the end of the window.
///
/// Both halves wrap, they are `unsigned short` in zsync. The point of the
/// thing is [`Rsum::roll`]: moving the window on by one byte costs two
/// additions instead of a pass over the whole block.
///
/// Follows zsync 0.6.2, `librcksum/rsum.c`: `rcksum_calc_rsum_block` at line
/// 41 and the `UPDATE_RSUM` macro at line 37.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rsum {
    a: u16,
    b: u16,
}

impl Rsum {
    /// The checksum of a window, computed from scratch. `b` weights the first
    /// byte by the window length and the last one by one, which is what makes
    /// a shifted window a different checksum.
    pub fn of(window: &[u8]) -> Self {
        let mut a = 0u16;
        let mut b = 0u16;
        // zsync counts the length down as it goes; the multiplication is
        // modulo 2^16 either way.
        let mut weight = window.len() as u16;

        for &byte in window {
            a = a.wrapping_add(u16::from(byte));
            b = b.wrapping_add(weight.wrapping_mul(u16::from(byte)));
            weight = weight.wrapping_sub(1);
        }
        Self { a, b }
    }

    /// Moves the window on by one byte: `leaving` drops off the front,
    /// `entering` joins at the back. `blockshift` is the log2 of the window
    /// length, which is why zsync insists on a power of two block size.
    ///
    /// `b` is updated with the *new* `a`, and the byte that leaves is
    /// subtracted weighted by the whole window length. Both are what the
    /// macro does, and getting either wrong stays invisible until a scan
    /// finds nothing.
    pub fn roll(&mut self, leaving: u8, entering: u8, blockshift: u32) {
        self.a = self.a.wrapping_add(u16::from(entering)).wrapping_sub(u16::from(leaving));
        // `leaving << blockshift` is `leaving * blocksize`. A window of 65536
        // bytes or more shifts every bit out, and the term is zero; shifting
        // a `u16` that far would panic instead.
        let weighted = if blockshift < 16 { u16::from(leaving) << blockshift } else { 0 };
        self.b = self.b.wrapping_add(self.a).wrapping_sub(weighted);
    }

    /// The two halves as one number, `a` above `b`, which is the order the
    /// block table stores them in.
    pub fn value(self) -> u32 {
        (u32::from(self.a) << 16) | u32::from(self.b)
    }
}

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
    use std::cell::Cell;
    use std::rc::Rc;

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
    fn control_bytes(length: u64, blocksize: u64, lengths: Option<&str>, table: &[u8]) -> Vec<u8> {
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

    /// The buffer the reference harness was run over: 4096 bytes, then the
    /// zeroes zsync pads a source file with at EOF.
    fn rolling_fixture() -> Vec<u8> {
        let mut data: Vec<u8> =
            (0..4096u32).map(|i| ((i * 37 + (i >> 3) * 11) % 251) as u8).collect();
        data.extend(std::iter::repeat_n(0u8, 1024));
        data
    }

    #[test]
    fn rolling_the_window_forward_matches_computing_it_from_scratch() {
        let data = rolling_fixture();
        for blocksize in [16usize, 64, 1024] {
            let blockshift = blocksize.trailing_zeros();
            let mut rolled = Rsum::of(&data[..blocksize]);

            // Every window that starts on one of the 4096 real bytes, which
            // is exactly how far zsync scans: the last ones run into the
            // zero padding.
            for start in 0..4096 {
                rolled.roll(data[start], data[start + blocksize], blockshift);
                let at = start + 1;
                let fresh = Rsum::of(&data[at..at + blocksize]);
                assert_eq!(rolled, fresh, "blocksize {blocksize}, offset {at}");
            }
        }
    }

    #[test]
    fn agrees_with_the_reference_implementation() {
        // Values printed by zsync 0.6.2's own rcksum_calc_rsum_block and
        // UPDATE_RSUM, compiled and run over the same buffer.
        let data = rolling_fixture();
        /// A window of the fixture: where it starts, and the two halves
        /// the reference printed for it.
        type Window = (usize, u16, u16);

        let expected: [(usize, [Window; 6]); 3] = [
            (
                16,
                [
                    (0, 1767, 13508),
                    (1, 1879, 15387),
                    (2, 1991, 16786),
                    (16, 2053, 17445),
                    (4080, 1907, 16706),
                    (4095, 10, 160),
                ],
            ),
            (
                64,
                [
                    (0, 7529, 47145),
                    (1, 7726, 54871),
                    (2, 7923, 60426),
                    (64, 7838, 59823),
                    (4032, 7669, 57719),
                    (4095, 10, 640),
                ],
            ),
            (
                1024,
                [
                    (0, 62390, 33003),
                    (1, 62530, 29997),
                    (2, 62670, 54779),
                    (1024, 62429, 53066),
                    (3072, 62256, 7074),
                    (4095, 10, 10240),
                ],
            ),
        ];

        for (blocksize, windows) in expected {
            for (at, a, b) in windows {
                let fresh = Rsum::of(&data[at..at + blocksize]);
                assert_eq!(
                    fresh.value(),
                    (u32::from(a) << 16) | u32::from(b),
                    "blocksize {blocksize}, offset {at}"
                );
            }
        }
    }

    #[test]
    fn a_window_running_past_the_end_of_the_file_reads_zeroes() {
        // zsync pads the source with blocksize * seq_matches zeroes at EOF,
        // so the last window that starts on a real byte is one byte of file
        // and the rest padding. Rolling into that must give the same answer
        // as hashing the padded slice.
        let data = rolling_fixture();
        let blocksize = 1024usize;
        let last = 4095;
        let mut rolled = Rsum::of(&data[..blocksize]);
        for start in 0..last {
            rolled.roll(data[start], data[start + blocksize], blocksize.trailing_zeros());
        }

        assert_eq!(rolled, Rsum::of(&data[last..last + blocksize]));
        // One byte of file, 1023 zeroes: a is that byte, b is it weighted by
        // the whole window.
        assert_eq!(rolled.value(), (10 << 16) | 10_240);
    }

    #[test]
    fn a_local_file_shorter_than_a_block_still_has_a_checksum() {
        // Padded to the block size, which is what a scan does with a short
        // file rather than skipping it.
        let mut window = b"tiny".to_vec();
        window.resize(16, 0);
        assert_eq!(Rsum::of(&window), Rsum::of(b"tiny\0\0\0\0\0\0\0\0\0\0\0\0"));
        // An empty window is not a panic.
        assert_eq!(Rsum::of(&[]), Rsum::default());
    }

    #[test]
    fn a_rolled_checksum_wraps_the_way_a_pair_of_shorts_does() {
        // 300 bytes of 0xff overflow both halves several times over.
        let data = vec![0xffu8; 512];
        let blocksize = 256usize;
        let mut rolled = Rsum::of(&data[..blocksize]);
        for start in 0..(data.len() - blocksize) {
            rolled.roll(data[start], data[start + blocksize], blocksize.trailing_zeros());
            assert_eq!(rolled, Rsum::of(&data[start + 1..start + 1 + blocksize]));
        }
    }

    #[test]
    fn a_window_of_65536_bytes_does_not_shift_a_u16_out_of_range() {
        // blockshift is 16 here, and the term zsync subtracts is zero modulo
        // 2^16. Shifting a u16 by 16 would panic instead.
        let data: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let blocksize = 65_536usize;
        let mut rolled = Rsum::of(&data[..blocksize]);
        for start in 0..64 {
            rolled.roll(data[start], data[start + blocksize], blocksize.trailing_zeros());
        }
        assert_eq!(rolled, Rsum::of(&data[64..64 + blocksize]));
    }

    #[test]
    fn a_computed_checksum_is_truncated_the_way_the_table_stores_one() {
        let rsum = Rsum { a: 0xabcd, b: 0x1234 };
        let lengths = |rsum_bytes| HashLengths { seq_matches: 1, rsum_bytes, checksum_bytes: 4 };

        // Four bytes: the whole thing. Three: one byte of a. Two: b alone.
        assert_eq!(lengths(4).truncate(rsum), 0xabcd_1234);
        assert_eq!(lengths(3).truncate(rsum), 0x00cd_1234);
        assert_eq!(lengths(2).truncate(rsum), 0x0000_1234);
        // One byte is zsync's own oddity: it still compares a whole b, so a
        // stored value of eight bits can only match a b that fits in eight.
        assert_eq!(lengths(1).truncate(rsum), 0x0000_1234);
        assert_eq!(lengths(1).truncate(Rsum { a: 0xabcd, b: 0x0034 }), 0x34);
    }

    #[test]
    fn a_table_entry_and_a_computed_checksum_meet_in_the_same_number() {
        // What stage four will compare: the value read out of the table
        // against the truncated checksum of a window.
        let table: Vec<u8> = vec![0x12, 0x34, 1, 2, 3, 4];
        let control = parse_control(&control_bytes(2048, 2048, Some("1,2,4"), &table)).unwrap();
        assert_eq!(control.blockshift(), 11);

        let mut block = vec![0u8; 2048];
        block[0] = 0x9a;
        block[7] = 0x5b;
        let computed = control.hash_lengths.truncate(Rsum::of(&block));
        assert_eq!(computed, Rsum::of(&block).value() & 0xffff);
        // The fixture's entry is 0x1234, so a window has to reach that value
        // to be worth an MD4.
        assert_eq!(control.blocks[0].rsum, 0x1234);
        assert_ne!(computed, control.blocks[0].rsum);
    }

    /// One byte of a stream that does not repeat, so a block of it only
    /// matches where it belongs. A function of the offset, so the same bytes
    /// can be produced in a slice or one piece at a time.
    fn noise_byte(seed: u64, at: u64) -> u8 {
        let mut x = seed ^ at.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
        (x >> 31) as u8
    }

    fn noise(seed: u64, len: usize) -> Vec<u8> {
        (0..len as u64).map(|at| noise_byte(seed, at)).collect()
    }

    /// The zsync file a target of these bytes would ship with, written with
    /// the checksums this module computes. The last block is checksummed
    /// zero padded, as zsync does.
    fn control_for(target: &[u8], blocksize: usize, lengths: &str) -> ControlFile {
        let hash_lengths = HashLengths::parse(lengths).unwrap();
        let mut table = Vec::new();

        for start in (0..target.len()).step_by(blocksize) {
            let mut window = vec![0u8; blocksize];
            let end = (start + blocksize).min(target.len());
            window[..end - start].copy_from_slice(&target[start..end]);

            let rsum = Rsum::of(&window).value().to_be_bytes();
            table.extend_from_slice(&rsum[4 - hash_lengths.rsum_bytes as usize..]);
            table.extend_from_slice(&md4(&window)[..hash_lengths.checksum_bytes as usize]);
        }

        let file = control_bytes(target.len() as u64, blocksize as u64, Some(lengths), &table);
        parse_control(&file).unwrap()
    }

    /// Every block of the target, at the offset a block of that number sits
    /// at in a file that is the target itself.
    fn offsets_in_order(map: &SourceMap, blocksize: u64) -> Vec<Option<u64>> {
        (0..map.blocks())
            .map(|block| map.offset_of(block))
            .collect::<Vec<_>>()
            .into_iter()
            .zip(0..)
            .map(|(offset, block): (Option<u64>, u64)| {
                offset.map(|at| at.wrapping_sub(block * blocksize))
            })
            .collect()
    }

    #[test]
    fn a_seed_that_is_already_the_target_holds_every_block() {
        // Ten whole blocks and a short one, which is checksummed zero padded
        // and has to match that way.
        let target = noise(1, 10 * 1024 + 123);
        let control = control_for(&target, 1024, "2,2,4");
        assert_eq!(control.blocks.len(), 11);

        let map = scan(&control, target.as_slice()).unwrap();

        assert_eq!(map.matched(), 11);
        assert!(map.is_complete());
        assert_eq!(map.offset_of(0), Some(0));
        assert_eq!(map.offset_of(10), Some(10 * 1024));
        // Every block found exactly where it sits.
        assert!(offsets_in_order(&map, 1024).iter().all(|shift| *shift == Some(0)));
    }

    #[test]
    fn a_seed_that_shares_nothing_holds_no_block() {
        let target = noise(1, 12 * 1024);
        let other = noise(2, 12 * 1024);
        let control = control_for(&target, 1024, "2,2,4");

        let map = scan(&control, other.as_slice()).unwrap();

        assert_eq!(map.matched(), 0);
        assert!(!map.is_complete());
        assert_eq!(map.offset_of(0), None);
        assert_eq!(map.blocks(), 12);
    }

    #[test]
    fn finds_exactly_the_blocks_the_two_files_share() {
        // The first eight blocks of the target, moved 1000 bytes along by a
        // header that grew: the offsets are no multiple of the block size,
        // which is the whole reason for a rolling checksum.
        let blocksize = 1024;
        let target = noise(1, 12 * blocksize + 500);
        let control = control_for(&target, blocksize, "2,2,4");
        assert_eq!(control.blocks.len(), 13);

        let mut seed = noise(7, 1000);
        seed.extend_from_slice(&target[..8 * blocksize]);
        seed.extend(noise(8, 700));

        let map = scan(&control, seed.as_slice()).unwrap();

        assert_eq!(map.matched(), 8);
        for block in 0..8u64 {
            assert_eq!(map.offset_of(block as usize), Some(1000 + block * blocksize as u64));
        }
        for block in 8..13 {
            assert_eq!(map.offset_of(block), None, "block {block}");
        }
    }

    #[test]
    fn finds_blocks_that_moved_within_the_seed() {
        // The two halves of the target, swapped. Every block is present, but
        // no block is where it was.
        let blocksize = 1024;
        let target = noise(1, 12 * blocksize);
        let control = control_for(&target, blocksize, "2,2,4");

        let mut seed = target[6 * blocksize..].to_vec();
        seed.extend_from_slice(&target[..6 * blocksize]);

        let map = scan(&control, seed.as_slice()).unwrap();

        assert_eq!(map.matched(), 12);
        assert_eq!(map.offset_of(6), Some(0));
        assert_eq!(map.offset_of(0), Some(6 * blocksize as u64));
        // The last block is not in the lookup table when blocks have to
        // match in sequence; it is found by the run that reaches it.
        assert_eq!(map.offset_of(11), Some(5 * blocksize as u64));
    }

    #[test]
    fn the_same_bytes_can_serve_two_blocks_of_the_target() {
        // A target that repeats one pair of blocks, and a seed that holds
        // that pair once.
        let blocksize = 1024;
        let mut target = noise(1, 10 * blocksize);
        let repeated = target[2 * blocksize..4 * blocksize].to_vec();
        target[6 * blocksize..8 * blocksize].copy_from_slice(&repeated);
        let control = control_for(&target, blocksize, "2,2,4");

        let mut seed = noise(7, 300);
        seed.extend_from_slice(&repeated);
        seed.extend(noise(8, 300));

        let map = scan(&control, seed.as_slice()).unwrap();

        assert_eq!(map.matched(), 4);
        assert_eq!(map.offset_of(2), Some(300));
        assert_eq!(map.offset_of(3), Some(300 + blocksize as u64));
        assert_eq!(map.offset_of(6), Some(300));
        assert_eq!(map.offset_of(7), Some(300 + blocksize as u64));
    }

    #[test]
    fn a_block_that_changed_costs_that_block_and_no_more() {
        // What an update actually looks like: the same file with one block
        // rewritten. The run of matches breaks there and picks up again on
        // the next pair.
        let blocksize = 1024;
        let mut seed = noise(1, 12 * blocksize);
        let control = control_for(&seed, blocksize, "2,2,4");
        seed[5 * blocksize..6 * blocksize].fill(0x5a);

        let map = scan(&control, seed.as_slice()).unwrap();

        assert_eq!(map.matched(), 11);
        assert_eq!(map.offset_of(5), None);
        assert_eq!(map.offset_of(4), Some(4 * blocksize as u64));
        assert_eq!(map.offset_of(6), Some(6 * blocksize as u64));
        assert_eq!(map.offset_of(11), Some(11 * blocksize as u64));
    }

    #[test]
    fn a_lone_block_is_found_only_when_the_header_asks_for_one_match() {
        let blocksize = 1024;
        let target = noise(1, 8 * blocksize);
        let mut seed = noise(7, 400);
        seed.extend_from_slice(&target[3 * blocksize..4 * blocksize]);
        seed.extend(noise(8, 400));

        // Hash-Lengths of 1,2,4: one block on its own is a match.
        let one = control_for(&target, blocksize, "1,2,4");
        let map = scan(&one, seed.as_slice()).unwrap();
        assert_eq!(map.matched(), 1);
        assert_eq!(map.offset_of(3), Some(400));

        // 2,2,4: two blocks in a row have to match, and there is only one.
        let two = control_for(&target, blocksize, "2,2,4");
        assert_eq!(scan(&two, seed.as_slice()).unwrap().matched(), 0);
    }

    #[test]
    fn a_seed_with_nothing_much_in_it_is_not_a_panic() {
        let blocksize = 1024;
        let target = noise(1, 4 * blocksize);
        let control = control_for(&target, blocksize, "2,2,4");

        // Shorter than one block, and shorter than the window the scan needs.
        assert_eq!(scan(&control, &target[..100]).unwrap().matched(), 0);
        assert_eq!(scan(&control, &[][..]).unwrap().matched(), 0);
        // A single byte of a file, and a file of zeroes, which is what a
        // truncated download looks like.
        assert_eq!(scan(&control, &b"x"[..]).unwrap().matched(), 0);
        assert_eq!(scan(&control, vec![0u8; 5000].as_slice()).unwrap().matched(), 0);

        // A target of a single block: with two matches required there is no
        // pair to find, which is what the reference ends up doing too.
        let tiny = noise(1, 900);
        assert_eq!(
            scan(&control_for(&tiny, blocksize, "2,2,4"), tiny.as_slice()).unwrap().matched(),
            0
        );
        assert_eq!(
            scan(&control_for(&tiny, blocksize, "1,2,4"), tiny.as_slice()).unwrap().matched(),
            1
        );
    }

    #[test]
    fn scans_a_file_on_disk() {
        let blocksize = 1024;
        let target = noise(1, 6 * blocksize + 77);
        let control = control_for(&target, blocksize, "2,2,4");

        let file = tempfile::NamedTempFile::new().unwrap();
        let mut seed = noise(9, 333);
        seed.extend_from_slice(&target[..4 * blocksize]);
        std::fs::write(file.path(), &seed).unwrap();

        let map = scan_file(&control, file.path()).unwrap();
        assert_eq!(map.matched(), 4);
        assert_eq!(map.offset_of(0), Some(333));
        assert_eq!(map.offset_of(4), None);

        // A file that is not there is an error naming it, not a panic.
        let missing = file.path().with_extension("gone");
        let error = scan_file(&control, &missing).unwrap_err();
        assert!(error.to_string().contains("gone"), "{error}");
    }

    /// How much memory this process holds, in bytes. The second field of
    /// `/proc/self/statm` is the resident set in pages.
    fn resident_bytes() -> u64 {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
        let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
        pages * 4096
    }

    /// A reader that makes up a long file as it goes, remembers the largest
    /// slice it was ever asked to fill, and watches how much memory the
    /// process holds while the scan runs.
    struct StreamedSeed {
        seed: u64,
        junk: u64,
        len: u64,
        at: u64,
        largest_read: Rc<Cell<usize>>,
        served: Rc<Cell<u64>>,
        peak_resident: Rc<Cell<u64>>,
    }

    impl Read for StreamedSeed {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.largest_read.set(self.largest_read.get().max(buf.len()));
            self.peak_resident.set(self.peak_resident.get().max(resident_bytes()));
            if self.at >= self.len {
                return Ok(0);
            }

            // Short reads on purpose: a scan has to cope with a stream that
            // hands over less than it asked for.
            let take = buf.len().min((self.len - self.at) as usize).min(9973);
            for (slot, at) in buf[..take].iter_mut().zip(self.at..) {
                *slot = if at < self.junk { 0xa5 } else { noise_byte(self.seed, at - self.junk) };
            }
            self.at += take as u64;
            self.served.set(self.served.get() + take as u64);
            Ok(take)
        }
    }

    #[test]
    fn a_large_seed_is_read_in_pieces_and_never_held_whole() {
        // 24 MB of file, which no part of this test ever materialises: the
        // target is checksummed a block at a time and the seed is made up as
        // the scan reads it.
        let blocksize = 4096usize;
        let blocks = 6 * 1024;
        let length = blocks * blocksize;
        let junk = 1000u64;

        let mut table = Vec::with_capacity(blocks * 7);
        let mut window = vec![0u8; blocksize];
        for block in 0..blocks {
            for (slot, at) in window.iter_mut().zip((block * blocksize) as u64..) {
                *slot = noise_byte(1, at);
            }
            let rsum = Rsum::of(&window).value().to_be_bytes();
            table.extend_from_slice(&rsum[2..]);
            table.extend_from_slice(&md4(&window)[..5]);
        }
        let control =
            parse_control(&control_bytes(length as u64, blocksize as u64, Some("2,2,5"), &table))
                .unwrap();

        let largest_read = Rc::new(Cell::new(0usize));
        let served = Rc::new(Cell::new(0u64));
        let peak_resident = Rc::new(Cell::new(0u64));
        let seed = StreamedSeed {
            seed: 1,
            junk,
            len: junk + length as u64,
            at: 0,
            largest_read: Rc::clone(&largest_read),
            served: Rc::clone(&served),
            peak_resident: Rc::clone(&peak_resident),
        };

        let before = resident_bytes();
        let map = scan(&control, seed).unwrap();

        // The whole target is in there, 1000 bytes along.
        assert_eq!(map.matched(), blocks);
        assert!(map.is_complete());
        assert_eq!(map.offset_of(0), Some(junk));
        assert_eq!(map.offset_of(blocks - 1), Some(junk + (blocks - 1) as u64 * blocksize as u64));

        // The scan asked for the file in pieces of the size it buffers, and
        // never more.
        assert!(
            largest_read.get() <= SCAN_CHUNK,
            "the scan asked for {} bytes at once, more than the {SCAN_CHUNK} it buffers",
            largest_read.get()
        );
        // And it read the file once, not twice.
        assert_eq!(served.get(), junk + length as u64);

        // What the reader saw of this process while the scan ran. Holding
        // the 24 MB seed, as a read_to_end would, shows up here; the block
        // table and one buffer do not. The allowance is wide because other
        // tests are allocating in the same process at the same time.
        let growth = peak_resident.get().saturating_sub(before);
        assert!(
            growth < 8 * 1024 * 1024,
            "the scan held {growth} bytes more than it started with, for a seed of {} bytes",
            junk + length as u64
        );
    }
}
