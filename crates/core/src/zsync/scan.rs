//! The scan: which blocks of the complete file a local file already holds.
//!
//! Structure follows zsync 0.6.2, `librcksum/rsum.c:301` onwards: a lookup by
//! rolling checksum, MD4 to settle it, then a jump of a whole block once the
//! bytes turn out to be a block of the target.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::error::{Error, Result};

use super::control::ControlFile;
use super::md4::md4;
use super::rsum::Rsum;

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

    /// A map with the given blocks found, for tests that want to pick which
    /// blocks are missing rather than scan for them.
    #[cfg(test)]
    pub(crate) fn from_found(blocks: usize, found: &[usize]) -> Self {
        let mut map = Self::new(blocks);
        for &block in found {
            map.record(block, block as u64);
        }
        map
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::super::control::tests::control_bytes;
    use super::super::control::{parse_control, ControlFile, HashLengths};
    use super::super::md4::md4;
    use super::super::rsum::Rsum;
    use super::*;

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
