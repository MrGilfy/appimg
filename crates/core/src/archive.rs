//! AppImages that a project ships inside an archive, a zip file or a tar
//! file, plain or gzip-compressed, the way HarbourMasters/Shipwright ships
//! `soh.appimage` inside `SoH-<codename>-Linux.zip`.
//!
//! The AppImage is found by its magic bytes, never by its name: the ELF
//! header with `AI` and the AppImage type behind it, see
//! [`metadata::appimage_magic`]. Exactly one entry has to carry them. Beyond
//! that the archive decides nothing. No name, path, link, mode or time out
//! of it is used to write anything: the one AppImage goes to the file the
//! caller names and nowhere else, and it may unpack to no more than
//! [`limit_for`] allows, whatever the archive claims about it.
//!
//! Both formats are read here, on top of `flate2`, which the HTTP client
//! builds in anyway for gzip-compressed responses: zip entries that are
//! stored or deflated, and tar files in the ustar, GNU and pax flavours.
//! Encrypted zip entries and zip64 archives are refused.

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use flate2::read::{DeflateDecoder, MultiGzDecoder};
use flate2::Crc;

use crate::error::{Error, Result};
use crate::fs_util::{self, human_size, MODE_EXEC};
use crate::metadata;

/// How much of an entry tells whether it is an AppImage.
const HEAD: usize = 11;

/// The archive formats appimg looks into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    Tar,
    TarGz,
}

impl Kind {
    /// What a file is, by its first bytes, whatever its name says.
    pub fn of(path: &Path) -> Option<Kind> {
        let mut head = [0u8; 262];
        let mut file = File::open(path).ok()?;
        let read = read_up_to(&mut file, &mut head).ok()?;
        let head = &head[..read];
        if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
            Some(Kind::Zip)
        } else if head.starts_with(&[0x1f, 0x8b]) {
            Some(Kind::TarGz)
        } else if head.len() == 262 && &head[257..262] == b"ustar" {
            Some(Kind::Tar)
        } else {
            None
        }
    }
}

/// The endings of the archive names a release may ship an AppImage in.
const ARCHIVE_SUFFIXES: &[&str] = &[".tar.gz", ".tgz", ".tar", ".zip"];

/// `name` without the ending that makes it an archive, when it has one.
pub fn strip_archive_suffix(name: &str) -> Option<&str> {
    ARCHIVE_SUFFIXES.iter().find_map(|suffix| {
        let cut = name.len().checked_sub(suffix.len())?;
        (name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(suffix))
            .then(|| &name[..cut])
    })
}

/// Whether a file name is that of an archive appimg can look into.
pub fn is_archive_name(name: &str) -> bool {
    strip_archive_suffix(name).is_some()
}

/// The most the AppImage out of an archive of `archive_len` bytes may unpack
/// to: four times the archive, and 64 MiB more. An AppImage is a squashfs
/// image, compressed already, so packing it into an archive barely shrinks
/// it, and a real one stays far below that. A broken or hostile archive that
/// would unpack to more is stopped there, before it fills the disk.
pub fn limit_for(archive_len: u64) -> u64 {
    archive_len.saturating_mul(4).saturating_add(64 << 20)
}

/// The AppImage an archive held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    /// Its name inside the archive, for messages only, with anything a
    /// terminal would act on replaced.
    pub entry: String,
    pub size: u64,
}

/// Finds the one AppImage in `archive` and writes it to `dest`, executable,
/// within [`limit_for`] the size of the archive.
pub fn extract_appimage(archive: &Path, dest: &Path) -> Result<Extracted> {
    let len = fs::metadata(archive).map_err(|e| Error::io(archive, e))?.len();
    extract_appimage_within(archive, dest, limit_for(len))
}

/// [`extract_appimage`] with the limit given. Whatever fails, `dest` is
/// gone again before the error comes back.
pub fn extract_appimage_within(archive: &Path, dest: &Path, limit: u64) -> Result<Extracted> {
    let unpacked = match Kind::of(archive) {
        None => Err("it is no zip or tar archive".to_string()),
        Some(Kind::Zip) => zip::extract(archive, dest, limit),
        Some(kind) => {
            File::open(archive).map_err(|e| format!("it cannot be read: {e}")).and_then(|file| {
                let file = BufReader::new(file);
                match kind {
                    Kind::TarGz => tar::extract(MultiGzDecoder::new(file), dest, limit),
                    _ => tar::extract(file, dest, limit),
                }
            })
        }
    };
    let unpacked = unpacked.and_then(|extracted| {
        fs_util::set_mode(dest, MODE_EXEC).map_err(|e| e.to_string())?;
        Ok(extracted)
    });
    unpacked.map_err(|reason| {
        let _ = fs::remove_file(dest);
        Error::Archive { archive: archive.display().to_string(), reason }
    })
}

fn is_appimage(head: &[u8]) -> bool {
    matches!(metadata::appimage_magic(head), Some(1 | 2))
}

/// Exactly one AppImage, or why not.
fn exactly_one(found: &[String], others: &[String]) -> std::result::Result<(), String> {
    match found.len() {
        1 => Ok(()),
        0 if others.is_empty() => Err("it holds no files".to_string()),
        0 => Err(format!("it holds no AppImage, only {}", listing(others))),
        count => Err(format!(
            "it holds {count} AppImages, and appimg takes exactly one: {}",
            listing(found)
        )),
    }
}

fn listing(names: &[String]) -> String {
    const SHOWN: usize = 10;
    let shown = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    match names.len().checked_sub(SHOWN) {
        Some(more) if more > 0 => format!("{shown} and {more} more"),
        _ => shown,
    }
}

/// An entry name as a message can show it: control characters, which a
/// terminal could act on, replaced, and cut short when it is long.
fn display_name(raw: &[u8]) -> String {
    let name: String = String::from_utf8_lossy(raw)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    match name.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &name[..cut]),
        None => name,
    }
}

fn too_large(name: &str, limit: u64) -> String {
    format!(
        "{name} would unpack to more than {} ({limit} bytes), which no AppImage in an archive \
         this size comes near: the archive is broken or was made to fill the disk",
        human_size(limit)
    )
}

/// Reads until `buffer` is full or the reader ends, and returns how much it
/// read.
fn read_up_to(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Writes `head` and then the rest of `reader` to `dest`, and gives up as
/// soon as that is more than `limit` bytes. Returns how many bytes it wrote
/// and their CRC-32.
fn write_capped(
    head: &[u8],
    reader: &mut dyn Read,
    dest: &Path,
    limit: u64,
    name: &str,
) -> std::result::Result<(u64, u32), String> {
    let cannot_write = |e: io::Error| format!("cannot write {}: {e}", dest.display());
    let mut file = File::create(dest).map_err(cannot_write)?;
    let mut crc = Crc::new();
    let mut written = head.len() as u64;
    crc.update(head);
    file.write_all(head).map_err(cannot_write)?;

    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("{name} cannot be unpacked: {error}")),
        };
        written += read as u64;
        if written > limit {
            return Err(too_large(name, limit));
        }
        crc.update(&buffer[..read]);
        file.write_all(&buffer[..read]).map_err(cannot_write)?;
    }
    file.flush().map_err(cannot_write)?;
    Ok((written, crc.sum()))
}

/// Zip files, as PKWARE's APPNOTE describes them: the central directory at
/// the end lists the entries, each with the offset of its local header.
mod zip {
    use super::*;

    const LOCAL: &[u8; 4] = b"PK\x03\x04";
    const CENTRAL: &[u8; 4] = b"PK\x01\x02";
    const END: &[u8; 4] = b"PK\x05\x06";
    const END64_LOCATOR: &[u8; 4] = b"PK\x06\x07";
    /// The end of central directory record without its comment.
    const END_LEN: usize = 22;
    const CENTRAL_LEN: usize = 46;
    const LOCAL_LEN: usize = 30;
    /// A central directory larger than this lists more entries than any
    /// archive an AppImage ships in.
    const MAX_DIRECTORY: u64 = 16 << 20;

    struct Entry {
        name: String,
        directory: bool,
        flags: u16,
        method: u16,
        crc: u32,
        compressed: u64,
        size: u64,
        offset: u64,
        zip64: bool,
    }

    pub(super) fn extract(
        path: &Path,
        dest: &Path,
        limit: u64,
    ) -> std::result::Result<Extracted, String> {
        let unreadable = |e: io::Error| format!("it cannot be read: {e}");
        let mut file = File::open(path).map_err(unreadable)?;
        let len = file.metadata().map_err(unreadable)?.len();
        let entries = central_directory(&mut file, len)?;
        let files: Vec<&Entry> = entries.iter().filter(|entry| !entry.directory).collect();

        // Nothing is looked at before every entry is one that can be read:
        // any of them could be the AppImage.
        for entry in &files {
            if entry.flags & 0x41 != 0 || entry.method == 99 {
                return Err(format!(
                    "the entry {} is encrypted, which appimg does not read",
                    entry.name
                ));
            }
            if entry.zip64 {
                return Err(format!(
                    "the entry {} is stored in zip64 form, which appimg does not read",
                    entry.name
                ));
            }
            if !matches!(entry.method, 0 | 8) {
                return Err(format!(
                    "the entry {} is compressed with method {}, and appimg reads stored and \
                     deflated entries only",
                    entry.name, entry.method
                ));
            }
        }

        let mut found = Vec::new();
        let mut others = Vec::new();
        for entry in &files {
            let mut head = [0u8; HEAD];
            let read = read_up_to(&mut data(&mut file, entry, len)?, &mut head)
                .map_err(|e| format!("{} cannot be unpacked: {e}", entry.name))?;
            if is_appimage(&head[..read]) {
                found.push(*entry);
            } else {
                others.push(entry.name.clone());
            }
        }
        let names: Vec<String> = found.iter().map(|entry| entry.name.clone()).collect();
        exactly_one(&names, &others)?;
        let entry = found[0];

        if entry.size > limit {
            return Err(too_large(&entry.name, limit));
        }
        let mut reader = data(&mut file, entry, len)?;
        let (written, crc) = write_capped(&[], &mut reader, dest, limit, &entry.name)?;
        if written != entry.size {
            return Err(format!(
                "{} unpacks to {written} bytes, and the archive says {}: it is damaged",
                entry.name, entry.size
            ));
        }
        if crc != entry.crc {
            return Err(format!(
                "{} does not match the checksum the archive holds for it: it is damaged",
                entry.name
            ));
        }
        Ok(Extracted { entry: entry.name.clone(), size: written })
    }

    /// The data of an entry, decompressed.
    fn data<'a>(
        file: &'a mut File,
        entry: &Entry,
        len: u64,
    ) -> std::result::Result<Box<dyn Read + 'a>, String> {
        let outside = || format!("the archive is damaged: {} lies outside it", entry.name);
        let mut header = [0u8; LOCAL_LEN];
        file.seek(SeekFrom::Start(entry.offset)).map_err(|_| outside())?;
        file.read_exact(&mut header).map_err(|_| outside())?;
        if &header[0..4] != LOCAL {
            return Err(format!("the archive is damaged: {} has no local header", entry.name));
        }
        let start = entry.offset + LOCAL_LEN as u64 + u16_at(&header, 26) + u16_at(&header, 28);
        if start.checked_add(entry.compressed).is_none_or(|end| end > len) {
            return Err(outside());
        }
        file.seek(SeekFrom::Start(start)).map_err(|_| outside())?;
        let raw = file.take(entry.compressed);
        Ok(match entry.method {
            0 => Box::new(raw),
            _ => Box::new(DeflateDecoder::new(raw)),
        })
    }

    fn central_directory(file: &mut File, len: u64) -> std::result::Result<Vec<Entry>, String> {
        let damaged = || "the archive is damaged: its central directory is cut short".to_string();
        let unreadable = |e: io::Error| format!("it cannot be read: {e}");

        // The end record sits at the very end, behind a comment of at most
        // 65535 bytes.
        let tail_len = len.min((END_LEN + 0xffff) as u64);
        let mut tail = vec![0u8; tail_len as usize];
        file.seek(SeekFrom::Start(len - tail_len)).map_err(unreadable)?;
        file.read_exact(&mut tail).map_err(unreadable)?;
        let at = (0..=tail.len().saturating_sub(END_LEN))
            .rev()
            .find(|&at| tail.len() >= at + END_LEN && &tail[at..at + 4] == END)
            .ok_or("it is no zip archive, it has no end of central directory")?;
        let end = &tail[at..at + END_LEN];
        let count = u16_at(end, 10);
        let size = u32_at(end, 12);
        let offset = u32_at(end, 16);

        let locator =
            at.checked_sub(20).is_some_and(|before| &tail[before..before + 4] == END64_LOCATOR);
        if locator || count == 0xffff || size == 0xffff_ffff || offset == 0xffff_ffff {
            return Err("it is a zip64 archive, which appimg does not read".to_string());
        }
        if u16_at(end, 4) != 0 || u16_at(end, 6) != 0 || u16_at(end, 8) != count {
            return Err("it is split across several files, which appimg does not read".to_string());
        }
        if size > MAX_DIRECTORY || offset.checked_add(size).is_none_or(|end| end > len) {
            return Err(damaged());
        }

        let mut directory = vec![0u8; size as usize];
        file.seek(SeekFrom::Start(offset)).map_err(unreadable)?;
        file.read_exact(&mut directory).map_err(unreadable)?;

        let mut entries = Vec::new();
        let mut at = 0;
        for _ in 0..count {
            let record = directory.get(at..at + CENTRAL_LEN).ok_or_else(damaged)?;
            if &record[0..4] != CENTRAL {
                return Err(damaged());
            }
            let name_len = u16_at(record, 28) as usize;
            let extra_len = u16_at(record, 30) as usize;
            let comment_len = u16_at(record, 32) as usize;
            let name_at = at + CENTRAL_LEN;
            let name = directory.get(name_at..name_at + name_len).ok_or_else(damaged)?;
            let extra = directory
                .get(name_at + name_len..name_at + name_len + extra_len)
                .ok_or_else(damaged)?;
            let (compressed, size, offset) =
                (u32_at(record, 20), u32_at(record, 24), u32_at(record, 42));
            entries.push(Entry {
                name: display_name(name),
                directory: name.ends_with(b"/"),
                flags: u16_at(record, 8) as u16,
                method: u16_at(record, 10) as u16,
                crc: u32_at(record, 16) as u32,
                compressed,
                size,
                offset,
                zip64: [compressed, size, offset].contains(&0xffff_ffff) || has_extra(extra, 1),
            });
            at = name_at + name_len + extra_len + comment_len;
        }
        Ok(entries)
    }

    /// Whether an extra field holds a block with this id: `1` is zip64's.
    fn has_extra(mut extra: &[u8], id: u64) -> bool {
        while extra.len() >= 4 {
            if u16_at(extra, 0) == id {
                return true;
            }
            let skip = 4 + u16_at(extra, 2) as usize;
            extra = extra.get(skip..).unwrap_or_default();
        }
        false
    }

    fn u16_at(bytes: &[u8], at: usize) -> u64 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]]) as u64
    }

    fn u32_at(bytes: &[u8], at: usize) -> u64 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as u64
    }
}

/// Tar files, read front to back in one pass, as a gzip stream has to be:
/// 512-byte headers, each followed by the data it announces, padded to the
/// next 512 bytes. ustar, GNU long names and pax records are understood
/// well enough to name entries in messages.
mod tar {
    use super::*;

    const BLOCK: u64 = 512;
    /// More than this in a GNU long name or a pax record is no name.
    const MAX_RECORD: u64 = 1 << 20;
    /// What may follow the end of a tar file, padding to its record size,
    /// before the gzip trailer that carries the checksum.
    const MAX_TRAILER: u64 = 1 << 20;

    pub(super) fn extract(
        mut reader: impl Read,
        dest: &Path,
        limit: u64,
    ) -> std::result::Result<Extracted, String> {
        let mut found: Vec<String> = Vec::new();
        let mut others: Vec<String> = Vec::new();
        let mut long_name: Option<String> = None;
        let mut first = true;

        loop {
            let mut header = [0u8; BLOCK as usize];
            let read = read_up_to(&mut reader, &mut header).map_err(damaged)?;
            if read == 0 || header.iter().all(|byte| *byte == 0) {
                break;
            }
            if read < header.len() {
                return Err(cut_short());
            }
            if !checksum_matches(&header) {
                return Err(if first {
                    "it is no tar archive".to_string()
                } else {
                    "the archive is damaged: a header does not match its checksum".to_string()
                });
            }
            first = false;

            let size = size_of(&header)?;
            let padding = size.next_multiple_of(BLOCK) - size;
            let name = long_name.take().unwrap_or_else(|| header_name(&header));
            match header[156] {
                kind @ (b'L' | b'x') => {
                    if size > MAX_RECORD {
                        return Err(format!("the archive is damaged: a name of {size} bytes"));
                    }
                    let mut record = vec![0u8; size as usize];
                    reader.read_exact(&mut record).map_err(|_| cut_short())?;
                    long_name = match kind {
                        b'L' => Some(display_name(record.split(|b| *b == 0).next().unwrap_or(&[]))),
                        _ => pax_path(&record),
                    };
                }
                b'0' | b'\0' | b'7' => {
                    let mut entry = (&mut reader).take(size);
                    let mut head = [0u8; HEAD];
                    let read = read_up_to(&mut entry, &mut head).map_err(damaged)?;
                    let mut taken = read as u64;
                    if is_appimage(&head[..read]) {
                        found.push(name.clone());
                        if found.len() == 1 {
                            if size > limit {
                                return Err(too_large(&name, limit));
                            }
                            let (written, _) =
                                write_capped(&head[..read], &mut entry, dest, limit, &name)?;
                            taken = written;
                        }
                    } else {
                        others.push(name);
                    }
                    taken += io::copy(&mut entry, &mut io::sink()).map_err(damaged)?;
                    if taken != size {
                        return Err(cut_short());
                    }
                }
                // Directories, links, devices, global pax records: nothing
                // to look at, only data to step over.
                _ => skip(&mut reader, size)?,
            }
            skip(&mut reader, padding)?;
        }

        // A gzip stream checks its checksum at its very end, which lies
        // behind the padding of the tar file. Read up to it, but not without
        // end.
        let rest =
            io::copy(&mut (&mut reader).take(MAX_TRAILER + 1), &mut io::sink()).map_err(damaged)?;
        if rest > MAX_TRAILER {
            return Err("more follows the end of the tar archive than padding".to_string());
        }
        exactly_one(&found, &others)?;
        Ok(Extracted { entry: found.remove(0), size: fs_util::file_size(dest).unwrap_or(0) })
    }

    fn damaged(error: io::Error) -> String {
        format!("the archive is damaged: {error}")
    }

    fn cut_short() -> String {
        "the archive is cut short".to_string()
    }

    fn skip(reader: &mut impl Read, bytes: u64) -> std::result::Result<(), String> {
        let skipped = io::copy(&mut reader.take(bytes), &mut io::sink()).map_err(damaged)?;
        if skipped == bytes {
            Ok(())
        } else {
            Err(cut_short())
        }
    }

    /// The header checksum: the sum of its bytes, the checksum field
    /// counted as spaces. Some old tars summed signed bytes.
    fn checksum_matches(header: &[u8]) -> bool {
        let Some(stored) = octal(&header[148..156]) else {
            return false;
        };
        let field = 148..156;
        let unsigned: u64 = header
            .iter()
            .enumerate()
            .map(|(at, byte)| if field.contains(&at) { 32 } else { *byte as u64 })
            .sum();
        let signed: i64 = header
            .iter()
            .enumerate()
            .map(|(at, byte)| if field.contains(&at) { 32 } else { *byte as i8 as i64 })
            .sum();
        stored == unsigned || stored as i64 == signed
    }

    /// The size field: octal digits, or a big-endian number behind a first
    /// byte with its top bit set, for sizes octal cannot hold.
    fn size_of(header: &[u8]) -> std::result::Result<u64, String> {
        let field = &header[124..136];
        if field[0] & 0x80 != 0 {
            let mut value: u64 = (field[0] & 0x7f) as u64;
            for byte in &field[1..] {
                value = value
                    .checked_mul(256)
                    .and_then(|value| value.checked_add(*byte as u64))
                    .ok_or("the archive is damaged: an entry is larger than anything can be")?;
            }
            return Ok(value);
        }
        octal(field).ok_or_else(|| "the archive is damaged: an entry has no size".to_string())
    }

    fn octal(field: &[u8]) -> Option<u64> {
        let text = std::str::from_utf8(field).ok()?;
        let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
        if text.is_empty() {
            return Some(0);
        }
        u64::from_str_radix(text, 8).ok()
    }

    fn header_name(header: &[u8]) -> String {
        let field = |range: std::ops::Range<usize>| {
            let bytes = &header[range];
            &bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())]
        };
        let name = field(0..100);
        let prefix = if &header[257..262] == b"ustar" { field(345..500) } else { &[] };
        if prefix.is_empty() {
            display_name(name)
        } else {
            display_name(&[prefix, b"/", name].concat())
        }
    }

    /// The `path` out of a pax record: lines of `<length> <key>=<value>\n`.
    fn pax_path(mut record: &[u8]) -> Option<String> {
        while !record.is_empty() {
            let space = record.iter().position(|b| *b == b' ')?;
            let length: usize = std::str::from_utf8(&record[..space]).ok()?.parse().ok()?;
            let line = record.get(space + 1..length)?;
            if let Some(value) = line.strip_prefix(b"path=") {
                return Some(display_name(value.strip_suffix(b"\n").unwrap_or(value)));
            }
            record = record.get(length..)?;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_are_told_by_their_name_and_by_their_bytes() {
        for name in ["SoH-Ackbar-Delta-Linux.zip", "app.TAR.GZ", "app.tgz", "app.tar"] {
            assert!(is_archive_name(name), "{name}");
        }
        for name in ["app.AppImage", "app.zsync", "app.zip.sha256", "zip"] {
            assert!(!is_archive_name(name), "{name}");
        }
        assert_eq!(strip_archive_suffix("SoH-Linux.Zip"), Some("SoH-Linux"));
        assert_eq!(strip_archive_suffix("app-1.0.tar.gz"), Some("app-1.0"));

        let dir = tempfile::tempdir().unwrap();
        let kind = |bytes: &[u8]| {
            let path = dir.path().join("file");
            fs::write(&path, bytes).unwrap();
            Kind::of(&path)
        };
        assert_eq!(kind(b"PK\x03\x04rest"), Some(Kind::Zip));
        assert_eq!(kind(b"\x1f\x8b\x08\x00"), Some(Kind::TarGz));
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(kind(&tar), Some(Kind::Tar));
        assert_eq!(kind(b"\x7fELF\x02\x01\x01\x00AI\x02"), None);
        assert_eq!(kind(b""), None);
    }

    #[test]
    fn the_limit_leaves_room_for_any_real_appimage() {
        assert_eq!(limit_for(0), 64 << 20);
        assert_eq!(limit_for(100 << 20), (400 << 20) + (64 << 20));
        assert_eq!(limit_for(u64::MAX), u64::MAX);
    }

    #[test]
    fn names_from_an_archive_cannot_drive_a_terminal() {
        assert_eq!(display_name(b"soh.appimage"), "soh.appimage");
        assert_eq!(display_name(b"evil\x1b]0;title\x07.AppImage"), "evil?]0;title?.AppImage");
        assert_eq!(display_name(&[b'a'; 300]).chars().count(), 201);
        assert_eq!(
            listing(&(0..12).map(|n| n.to_string()).collect::<Vec<_>>()),
            "0, 1, 2, 3, 4, 5, 6, 7, 8, 9 and 2 more"
        );
    }
}
