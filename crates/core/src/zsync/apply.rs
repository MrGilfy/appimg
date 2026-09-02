//! Building the complete file out of what is here and what is not.
//!
//! The blocks a scan found are copied out of the local file, the rest are
//! fetched, and the result is checked against the SHA-1 the zsync header
//! carries for the whole file. A file that does not match that checksum is
//! deleted rather than handed on: the block checksums are four or five bytes
//! and the ranges came from a server, so this is the check that decides
//! whether what was assembled is the file that was promised.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::download::ProgressFn;
use crate::error::{Error, Result};
use crate::fs_util::{self, MODE_EXEC};

use super::control::ControlFile;
use super::fetch::{self, FetchReport};
use super::scan::{self, SourceMap};
use super::sha1_file;

/// How much is copied out of the local file at a time.
const COPY_CHUNK: usize = 1024 * 1024;

/// What applying a delta did, for the caller to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Applied {
    /// Blocks the complete file has.
    pub blocks: usize,
    /// Blocks that were already on disk and cost no bytes.
    pub reused: usize,
    /// Bytes that came over the wire.
    pub fetched: u64,
    /// How many requests that took.
    pub requests: usize,
    /// Whether the server ignored the ranges, which turns the update into a
    /// plain download of the whole file.
    pub whole_file: bool,
}

/// Assembles the file `control` describes into `output`, reusing what
/// `local` already holds and fetching the rest from `url`.
///
/// The output is verified before this returns. On a mismatch, or on any
/// failure along the way, `output` is removed: a half-written or wrong file
/// must never be left where an update could install it.
pub fn apply(
    control: &ControlFile,
    url: &str,
    local: &Path,
    output: &Path,
    progress: Option<ProgressFn<'_>>,
) -> Result<Applied> {
    let result = assemble(control, url, local, output, progress);
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

fn assemble(
    control: &ControlFile,
    url: &str,
    local: &Path,
    output: &Path,
    progress: Option<ProgressFn<'_>>,
) -> Result<Applied> {
    // Nothing is worth assembling without the checksum that says whether it
    // came out right.
    let expected = control.header.sha1.clone().ok_or_else(|| Error::Zsync {
        url: url.to_string(),
        reason: "the zsync file carries no checksum of the complete file, so an update from it \
                 could not be verified"
            .to_string(),
    })?;

    let map = scan::scan_file(control, local)?;

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let source = File::open(local).map_err(|e| Error::io(local, e))?;
    let assembled = File::create(output).map_err(|e| Error::io(output, e))?;
    assembled.set_len(control.header.length).map_err(|e| Error::io(output, e))?;

    copy_reused(&source, &assembled, control, &map, local, output)?;
    let report = fetch_missing(control, url, &map, &assembled, output, progress)?;

    let found = sha1_file(output)?;
    if found != expected {
        return Err(Error::Zsync {
            url: url.to_string(),
            reason: format!(
                "the assembled file is checksummed {found}, the zsync file says {expected}: \
                 what the server sent is not the file it described"
            ),
        });
    }
    fs_util::set_mode(output, MODE_EXEC)?;

    Ok(Applied {
        blocks: map.blocks(),
        reused: map.matched(),
        fetched: report.received,
        requests: report.requests,
        whole_file: report.whole_file,
    })
}

/// Copies the blocks the scan found, runs of blocks that sit next to each
/// other in both files in one go.
fn copy_reused(
    source: &File,
    assembled: &File,
    control: &ControlFile,
    map: &SourceMap,
    local: &Path,
    output: &Path,
) -> Result<()> {
    let blocksize = control.blocksize;
    let length = control.header.length;
    let mut buffer = vec![0u8; COPY_CHUNK.max(blocksize as usize)];
    let mut block = 0usize;

    while block < map.blocks() {
        let Some(at) = map.offset_of(block) else {
            block += 1;
            continue;
        };

        // How many blocks carry on from here in both files at once.
        let mut run = 1;
        while block + run < map.blocks()
            && map.offset_of(block + run) == Some(at + run as u64 * blocksize)
        {
            run += 1;
        }

        let start = block as u64 * blocksize;
        let span = (run as u64 * blocksize).min(length - start);
        let mut done = 0u64;

        while done < span {
            let take = ((span - done) as usize).min(buffer.len());
            let piece = &mut buffer[..take];
            // The scan matches windows that run past the end of the local
            // file against zeroes, so a piece that reaches the end has to be
            // padded the same way.
            piece.fill(0);
            read_at(source, piece, at + done).map_err(|e| Error::io(local, e))?;
            assembled.write_all_at(piece, start + done).map_err(|e| Error::io(output, e))?;
            done += take as u64;
        }
        block += run;
    }
    Ok(())
}

/// Fills in everything the local file did not supply.
fn fetch_missing(
    control: &ControlFile,
    url: &str,
    map: &SourceMap,
    assembled: &File,
    output: &Path,
    progress: Option<ProgressFn<'_>>,
) -> Result<FetchReport> {
    let total: u64 =
        fetch::missing_ranges(control, map).iter().map(|range| range.end - range.start).sum();
    let mut done = 0u64;
    let mut progress = progress;

    fetch::fetch_missing(url, control, map, |offset, bytes| {
        assembled.write_all_at(bytes, offset).map_err(|e| Error::io(output, e))?;
        done += bytes.len() as u64;
        if let Some(report) = progress.as_mut() {
            report(done, Some(total));
        }
        Ok(())
    })
}

/// Reads as much of `into` as the file holds, leaving the rest as it was.
fn read_at(file: &File, into: &mut [u8], offset: u64) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < into.len() {
        match file.read_at(&mut into[filled..], offset + filled as u64)? {
            0 => return Ok(()),
            read => filled += read,
        }
    }
    Ok(())
}
