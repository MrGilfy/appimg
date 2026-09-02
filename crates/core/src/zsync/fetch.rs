//! Fetching the blocks a scan did not find.
//!
//! The blocks that are missing are turned into byte ranges of the complete
//! file, one request each. Nothing here writes a file: every piece that
//! arrives is handed to the caller with the offset it belongs at, which is
//! what the assembly does with it.

use std::io::Read;
use std::ops::Range;

use crate::download::{Ranged, Session};
use crate::error::{Error, Result};

use super::control::ControlFile;
use super::scan::SourceMap;

/// Two runs of missing blocks with less than this between them are fetched
/// as one range, re-fetching the bytes in the middle.
///
/// A request costs a round trip whatever it carries, so a gap this size is
/// cheaper to download again than to ask for separately. It is also what
/// keeps a scattered delta from turning into thousands of requests.
const MERGE_GAP: u64 = 64 * 1024;

/// How much of a range is read at a time.
const PIECE: usize = 64 * 1024;

/// What a fetch did. `received` is the number of bytes that actually came
/// over the wire, which is the number that says whether reusing local blocks
/// was worth anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FetchReport {
    pub received: u64,
    pub requests: usize,
    /// Whether the server ignored the ranges and the whole file was
    /// downloaded instead.
    pub whole_file: bool,
}

/// The byte ranges of the complete file that the local file did not supply,
/// with runs that are close together merged.
pub fn missing_ranges(control: &ControlFile, map: &SourceMap) -> Vec<Range<u64>> {
    let length = control.header.length;
    let mut ranges: Vec<Range<u64>> = Vec::new();

    for block in 0..map.blocks() {
        if map.offset_of(block).is_some() {
            continue;
        }

        let start = block as u64 * control.blocksize;
        // The last block of the file is a short one; there is nothing past
        // the end to ask for.
        let end = ((block as u64 + 1) * control.blocksize).min(length);

        match ranges.last_mut() {
            Some(last) if start - last.end < MERGE_GAP => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

/// Fetches everything the local file did not supply, handing each piece to
/// `write` with the offset in the complete file it belongs at.
///
/// A server that ignores the range and answers 200 with the whole file is
/// taken at its word: the download becomes a plain one, the whole file is
/// handed over from offset zero, and the report says so. That leaves the
/// caller with a complete file to verify rather than an error to recover
/// from, and it costs nothing extra, as the body was already on its way.
pub fn fetch_missing<W>(
    url: &str,
    control: &ControlFile,
    map: &SourceMap,
    mut write: W,
) -> Result<FetchReport>
where
    W: FnMut(u64, &[u8]) -> Result<()>,
{
    let mut report = FetchReport::default();
    // One session for the whole update: every range after the first reuses
    // the connection, and the redirect a release asset answers with is
    // resolved once instead of per request.
    let mut session = Session::new();

    for range in missing_ranges(control, map) {
        report.requests += 1;

        match session.range(url, range.start, range.end - 1)? {
            Ranged::Partial(reader) => {
                let wanted = range.end - range.start;
                let got = copy(url, reader, range.start, wanted, &mut write)?;
                if got != wanted {
                    return Err(Error::Download(format!(
                        "{url}: asked for {wanted} bytes from {}, the server sent {got}",
                        range.start
                    )));
                }
                report.received += got;
            }
            Ranged::Whole(reader) => {
                // No more requests: this one response is the entire file.
                let length = control.header.length;
                let got = copy(url, reader, 0, length, &mut write)?;
                if got != length {
                    return Err(Error::Download(format!(
                        "{url}: the server ignored the range and sent {got} bytes, not the \
                         {length} the zsync file describes"
                    )));
                }
                report.received += got;
                report.whole_file = true;
                return Ok(report);
            }
        }
    }
    Ok(report)
}

/// Reads at most `wanted` bytes and hands them on in pieces. Returns how
/// many arrived, which is short when the server stopped early.
fn copy<W>(url: &str, reader: Box<dyn Read>, offset: u64, wanted: u64, write: &mut W) -> Result<u64>
where
    W: FnMut(u64, &[u8]) -> Result<()>,
{
    let mut reader = reader.take(wanted);
    let mut buffer = vec![0u8; PIECE];
    let mut done = 0u64;

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                write(offset + done, &buffer[..read])?;
                done += read as u64;
            }
            Err(e) => return Err(Error::Download(format!("{url}: {e}"))),
        }
    }

    finish(reader.into_inner());
    Ok(done)
}

/// Reads the end of a body that has already given up everything that was
/// asked for.
///
/// A range holds exactly as many bytes as were asked for, so the reader
/// above stops at its limit without ever asking the body whether more is
/// coming. That last read is what hands the connection back to the pool, so
/// the next range can go down it instead of opening one of its own. Whatever
/// it returns says nothing about the bytes that were handed on, which the
/// caller has already counted.
fn finish(mut reader: Box<dyn Read>) {
    let mut ignored = [0u8; 1];
    let _ = reader.read(&mut ignored);
}

#[cfg(test)]
mod tests {
    use super::super::control::parse_control;
    use super::super::control::tests::control_bytes;
    use super::*;

    /// A control file for a target of `blocks` blocks of 2048 bytes, the
    /// last one `tail` bytes long. The table itself is never looked at here.
    fn control(blocks: usize, tail: u64) -> ControlFile {
        let length = (blocks as u64 - 1) * 2048 + tail;
        let table = vec![0u8; blocks * 6];
        parse_control(&control_bytes(length, 2048, Some("2,2,4"), &table)).unwrap()
    }

    #[test]
    fn a_map_that_found_everything_needs_no_range() {
        let control = control(8, 2048);
        let map = SourceMap::from_found(8, &(0..8).collect::<Vec<_>>());
        assert!(missing_ranges(&control, &map).is_empty());
    }

    #[test]
    fn a_map_that_found_nothing_needs_one_range_for_the_whole_file() {
        let control = control(8, 500);
        let map = SourceMap::from_found(8, &[]);
        // Right up to the end of the file, not to the end of the last block.
        assert_eq!(missing_ranges(&control, &map), vec![0..7 * 2048 + 500]);
    }

    #[test]
    fn blocks_next_to_each_other_are_asked_for_together() {
        let control = control(200, 2048);
        let found: Vec<usize> = (0..200).filter(|block| !(20..30).contains(block)).collect();
        let map = SourceMap::from_found(200, &found);

        assert_eq!(missing_ranges(&control, &map), vec![20 * 2048..30 * 2048]);
    }

    #[test]
    fn runs_with_a_small_gap_between_them_are_asked_for_together() {
        // Ten blocks between the two runs is 20480 bytes, less than a
        // request is worth: one range, not two.
        let control = control(200, 2048);
        let missing: Vec<usize> = (20..25).chain(35..40).collect();
        let found: Vec<usize> = (0..200).filter(|block| !missing.contains(block)).collect();
        let map = SourceMap::from_found(200, &found);

        assert_eq!(missing_ranges(&control, &map), vec![20 * 2048..40 * 2048]);
    }

    #[test]
    fn runs_far_apart_are_asked_for_separately() {
        // 64 blocks between them is 131072 bytes, well past the gap that is
        // worth re-fetching.
        let control = control(200, 2048);
        let missing: Vec<usize> = (20..25).chain(89..94).collect();
        let found: Vec<usize> = (0..200).filter(|block| !missing.contains(block)).collect();
        let map = SourceMap::from_found(200, &found);

        assert_eq!(
            missing_ranges(&control, &map),
            vec![20 * 2048..25 * 2048, 89 * 2048..94 * 2048]
        );
    }

    #[test]
    fn the_last_block_is_asked_for_only_as_far_as_the_file_goes() {
        let control = control(8, 123);
        let map = SourceMap::from_found(8, &[0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(missing_ranges(&control, &map), vec![7 * 2048..7 * 2048 + 123]);
    }
}
