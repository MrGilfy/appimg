use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const CLASS_64: u8 = 2;
const DATA_LITTLE_ENDIAN: u8 = 1;
const SECTION_HEADER_SIZE: usize = 64;

/// The squashfs magic, `0x73717368` stored little-endian.
pub const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";
/// The squashfs version whose superblock layout [`minimum_length`] reads.
const SQUASHFS_MAJOR: u16 = 4;
/// How much of a squashfs superblock [`minimum_length`] reads, and a floor
/// under what a complete AppImage carries behind its ELF part: a squashfs
/// superblock alone is 96 bytes, and an ISO 9660 image keeps its first
/// volume descriptor 32 KiB in.
const SUPERBLOCK_PREFIX: u64 = 48;

/// Whether a file starts with the ELF magic bytes. Every AppImage does,
/// whatever its architecture, because its runtime is an ELF binary.
pub fn has_magic(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    File::open(path).and_then(|mut file| file.read_exact(&mut magic)).is_ok() && &magic == ELF_MAGIC
}

/// Why a file is no complete AppImage, as far as its front can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unfit {
    /// It does not start with the ELF magic every AppImage starts with.
    NoElfHeader,
    /// It is shorter than its front says a complete file is, see
    /// [`minimum_length`].
    CutShort { minimum: u64, actual: u64 },
}

impl std::fmt::Display for Unfit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unfit::NoElfHeader => write!(f, "it is not an AppImage, it does not start with an ELF header"),
            Unfit::CutShort { minimum, actual } => write!(
                f,
                "it is cut short, a complete file is at least {minimum} bytes and this one is {actual}"
            ),
        }
    }
}

/// The checks a file gets before anything runs it or installs it, whether
/// it was downloaded or was on disk already: it starts the way every
/// AppImage does, and it is at least as long as its front says. Only the
/// front and the length are read, nothing is run.
pub fn check_whole(path: &Path) -> Result<(), Unfit> {
    if !has_magic(path) {
        return Err(Unfit::NoElfHeader);
    }
    if let Some(minimum) = minimum_length(path) {
        let actual = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if actual < minimum {
            return Err(Unfit::CutShort { minimum, actual });
        }
    }
    Ok(())
}

/// Where the ELF part of the file ends, which is where an AppImage keeps
/// its squashfs payload. Reading the header beats scanning for magic bytes,
/// because the magic can also appear inside the payload itself.
pub fn payload_offset(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let end = section_table_end(&mut file)?;
    let size = file.metadata().ok()?.len();
    (end < size).then_some(end)
}

/// How long a complete AppImage has to be at least, going by what its front
/// says. A file shorter than that was cut off.
///
/// The ELF header says where the section table ends, which is where the
/// payload starts, and a complete file holds both its section table and at
/// least [`SUPERBLOCK_PREFIX`] bytes of payload. A squashfs 4.0
/// superblock there adds the size of the filesystem behind it. A file can
/// be longer than that, because mksquashfs pads what it writes to a
/// multiple of 4 KiB.
///
/// `None` when the front says nothing about the length: the file is no
/// 64-bit little-endian ELF, its header is cut short or names no section
/// table, or the payload is no squashfs 4.0 image, as in a type 1 AppImage,
/// which carries an ISO 9660 image instead.
pub fn minimum_length(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let offset = section_table_end(&mut file)?;
    let size = file.metadata().ok()?.len();

    // A complete ELF contains its own section table.
    if size < offset {
        return Some(offset);
    }
    // Behind it comes a squashfs superblock or the rest of an ISO 9660
    // image, either way more than the part of a superblock read below.
    let floor = offset.checked_add(SUPERBLOCK_PREFIX)?;
    if size < floor {
        return Some(floor);
    }

    // The superblock is little-endian, laid out as `struct
    // squashfs_super_block` in squashfs-tools: the magic at 0, the major
    // version at 28, and `bytes_used` at 40. Version 3 keeps its major
    // version at 28 as well, but not its size at 40.
    let mut superblock = [0u8; SUPERBLOCK_PREFIX as usize];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut superblock).ok()?;
    if &superblock[0..4] != SQUASHFS_MAGIC || read_u16(&superblock, 28)? != SQUASHFS_MAJOR {
        return None;
    }
    offset.checked_add(read_u64(&superblock, 40)?)
}

/// Where the section table of a 64-bit little-endian ELF file ends, going
/// by its header alone, whether or not the file is that long. `None` for
/// any other file, a header cut short, or a header without a section table.
fn section_table_end(file: &mut File) -> Option<u64> {
    let mut ident = [0u8; 16];
    file.read_exact(&mut ident).ok()?;
    if &ident[0..4] != ELF_MAGIC || ident[4] != CLASS_64 || ident[5] != DATA_LITTLE_ENDIAN {
        return None;
    }

    let mut header = [0u8; 48];
    file.read_exact(&mut header).ok()?;
    let section_table_offset = read_u64(&header, 24)?;
    let section_entry_size = read_u16(&header, 42)? as u64;
    let section_count = read_u16(&header, 44)? as u64;

    let end = section_table_offset.checked_add(section_entry_size.checked_mul(section_count)?)?;
    (end > 0).then_some(end)
}

/// Reads one section out of a 64-bit little-endian ELF file. Anything else,
/// including malformed files, yields `None`. AppImage runtimes are x86_64
/// binaries, so no other ELF flavour needs to be understood here.
pub fn read_section(path: &Path, section_name: &str) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;

    let mut ident = [0u8; 16];
    file.read_exact(&mut ident).ok()?;
    if &ident[0..4] != ELF_MAGIC || ident[4] != CLASS_64 || ident[5] != DATA_LITTLE_ENDIAN {
        return None;
    }

    // ELF64 header after e_ident: type, machine, version, entry, phoff,
    // shoff, flags, ehsize, phentsize, phnum, shentsize, shnum, shstrndx.
    let mut header = [0u8; 48];
    file.read_exact(&mut header).ok()?;
    let section_table_offset = read_u64(&header, 24)?;
    let section_entry_size = read_u16(&header, 42)? as u64;
    let section_count = read_u16(&header, 44)? as u64;
    let name_table_index = read_u16(&header, 46)? as u64;

    if section_table_offset == 0
        || section_count == 0
        || section_entry_size < SECTION_HEADER_SIZE as u64
    {
        return None;
    }

    let name_table_header =
        read_section_header(&mut file, section_table_offset, section_entry_size, name_table_index)?;
    let name_table =
        read_at(&mut file, read_u64(&name_table_header, 24)?, read_u64(&name_table_header, 32)?)?;

    for index in 0..section_count {
        let header =
            read_section_header(&mut file, section_table_offset, section_entry_size, index)?;
        let name_offset = read_u32(&header, 0)? as usize;
        if c_string_at(&name_table, name_offset) == section_name {
            return read_at(&mut file, read_u64(&header, 24)?, read_u64(&header, 32)?);
        }
    }
    None
}

fn read_section_header(
    file: &mut File,
    table_offset: u64,
    entry_size: u64,
    index: u64,
) -> Option<[u8; SECTION_HEADER_SIZE]> {
    let mut header = [0u8; SECTION_HEADER_SIZE];
    file.seek(SeekFrom::Start(table_offset.checked_add(index.checked_mul(entry_size)?)?)).ok()?;
    file.read_exact(&mut header).ok()?;
    Some(header)
}

fn read_at(file: &mut File, offset: u64, size: u64) -> Option<Vec<u8>> {
    // A section header can claim any size, so refuse absurd allocations.
    const MAX_SECTION: u64 = 8 * 1024 * 1024;
    if size > MAX_SECTION {
        return None;
    }
    let mut buffer = vec![0u8; usize::try_from(size).ok()?];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut buffer).ok()?;
    Some(buffer)
}

fn c_string_at(buffer: &[u8], offset: usize) -> String {
    if offset >= buffer.len() {
        return String::new();
    }
    let tail = &buffer[offset..];
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    String::from_utf8_lossy(&tail[..end]).into_owned()
}

fn read_u16(buffer: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buffer.get(offset..offset + 2)?.try_into().ok()?))
}

fn read_u32(buffer: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buffer.get(offset..offset + 4)?.try_into().ok()?))
}

fn read_u64(buffer: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(buffer.get(offset..offset + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_util::{write_atomic, MODE_FILE};

    /// Builds a minimal ELF64 file with a string table and one named section.
    fn elf_with_section(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut names = vec![0u8];
        let name_offset = names.len();
        names.extend_from_slice(name.as_bytes());
        names.push(0);
        let shstrtab_name_offset = names.len();
        names.extend_from_slice(b".shstrtab\0");

        let header_size = 64usize;
        let payload_offset = header_size;
        let names_offset = payload_offset + payload.len();
        let table_offset = names_offset + names.len();

        let mut out = Vec::new();
        out.extend_from_slice(ELF_MAGIC);
        out.push(CLASS_64);
        out.push(DATA_LITTLE_ENDIAN);
        out.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&2u16.to_le_bytes()); // e_type
        out.extend_from_slice(&62u16.to_le_bytes()); // e_machine
        out.extend_from_slice(&1u32.to_le_bytes()); // e_version
        out.extend_from_slice(&0u64.to_le_bytes()); // e_entry
        out.extend_from_slice(&0u64.to_le_bytes()); // e_phoff
        out.extend_from_slice(&(table_offset as u64).to_le_bytes()); // e_shoff
        out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
        out.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
        out.extend_from_slice(&0u16.to_le_bytes()); // e_phentsize
        out.extend_from_slice(&0u16.to_le_bytes()); // e_phnum
        out.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
        out.extend_from_slice(&3u16.to_le_bytes()); // e_shnum
        out.extend_from_slice(&2u16.to_le_bytes()); // e_shstrndx

        out.extend_from_slice(payload);
        out.extend_from_slice(&names);

        let mut section_header = |name_off: u32, offset: u64, size: u64| {
            out.extend_from_slice(&name_off.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes()); // sh_type
            out.extend_from_slice(&0u64.to_le_bytes()); // sh_flags
            out.extend_from_slice(&0u64.to_le_bytes()); // sh_addr
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // sh_link
            out.extend_from_slice(&0u32.to_le_bytes()); // sh_info
            out.extend_from_slice(&1u64.to_le_bytes()); // sh_addralign
            out.extend_from_slice(&0u64.to_le_bytes()); // sh_entsize
        };

        section_header(0, 0, 0); // the mandatory null section
        section_header(name_offset as u32, payload_offset as u64, payload.len() as u64);
        section_header(shstrtab_name_offset as u32, names_offset as u64, names.len() as u64);
        out
    }

    #[test]
    fn reads_a_named_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.elf");
        write_atomic(
            &path,
            &elf_with_section(".upd_info", b"zsync|https://example.com/app.zsync"),
            MODE_FILE,
        )
        .unwrap();

        let data = read_section(&path, ".upd_info").unwrap();
        assert_eq!(data, b"zsync|https://example.com/app.zsync");
    }

    #[test]
    fn missing_sections_and_non_elf_files_yield_none() {
        let dir = tempfile::tempdir().unwrap();
        let elf = dir.path().join("fake.elf");
        write_atomic(&elf, &elf_with_section(".upd_info", b"data"), MODE_FILE).unwrap();
        assert!(read_section(&elf, ".comment").is_none());

        let script = dir.path().join("script.sh");
        write_atomic(&script, b"#!/bin/sh\necho hi\n", MODE_FILE).unwrap();
        assert!(read_section(&script, ".upd_info").is_none());
    }

    #[test]
    fn the_payload_starts_where_the_section_table_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.elf");
        let mut bytes = elf_with_section(".upd_info", b"data");
        let elf_size = bytes.len() as u64;
        bytes.extend_from_slice(b"hsqs");
        bytes.extend_from_slice(&[0u8; 64]);
        write_atomic(&path, &bytes, MODE_FILE).unwrap();

        assert_eq!(payload_offset(&path), Some(elf_size));
    }

    #[test]
    fn a_file_without_a_payload_has_no_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.elf");
        write_atomic(&path, &elf_with_section(".upd_info", b"data"), MODE_FILE).unwrap();
        assert_eq!(payload_offset(&path), None);

        let script = dir.path().join("script.sh");
        write_atomic(&script, b"#!/bin/sh\n", MODE_FILE).unwrap();
        assert_eq!(payload_offset(&script), None);
    }

    #[test]
    fn the_payload_ends_where_its_superblock_says() {
        let dir = tempfile::tempdir().unwrap();
        let elf = elf_with_section(".upd_info", b"data");
        let offset = elf.len() as u64;

        // Magic, the major version at 28 and bytes_used at 40, and the
        // rest of a 96-byte superblock.
        let superblock = |magic: &[u8; 4], major: u16, bytes_used: u64| {
            let mut out = elf.clone();
            out.extend_from_slice(magic);
            out.extend_from_slice(&[0u8; 24]);
            out.extend_from_slice(&major.to_le_bytes());
            out.extend_from_slice(&[0u8; 10]);
            out.extend_from_slice(&bytes_used.to_le_bytes());
            out.extend_from_slice(&[0u8; 48]);
            out
        };

        let path = dir.path().join("squashfs.AppImage");
        write_atomic(&path, &superblock(SQUASHFS_MAGIC, 4, 12_345), MODE_FILE).unwrap();
        assert_eq!(minimum_length(&path), Some(offset + 12_345));

        // A type 1 AppImage carries an ISO 9660 image, which has no
        // superblock to read a size from.
        let iso = dir.path().join("iso.AppImage");
        write_atomic(&iso, &superblock(b"\0\0\0\0", 4, 12_345), MODE_FILE).unwrap();
        assert_eq!(minimum_length(&iso), None);

        // Squashfs 3 keeps something else at byte 40.
        let old = dir.path().join("squashfs3.AppImage");
        write_atomic(&old, &superblock(SQUASHFS_MAGIC, 3, 12_345), MODE_FILE).unwrap();
        assert_eq!(minimum_length(&old), None);
    }

    #[test]
    fn only_a_file_that_starts_with_the_elf_magic_has_it() {
        let dir = tempfile::tempdir().unwrap();
        let elf = dir.path().join("fake.elf");
        write_atomic(&elf, &elf_with_section(".upd_info", b"data"), MODE_FILE).unwrap();
        assert!(has_magic(&elf));

        let page = dir.path().join("page.html");
        write_atomic(&page, b"<!DOCTYPE html>\n<html></html>\n", MODE_FILE).unwrap();
        assert!(!has_magic(&page));

        let short = dir.path().join("short");
        write_atomic(&short, &ELF_MAGIC[..3], MODE_FILE).unwrap();
        assert!(!has_magic(&short));

        assert!(!has_magic(&dir.path().join("missing")));
    }

    #[test]
    fn truncated_files_do_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.elf");
        let mut bytes = elf_with_section(".upd_info", b"data");
        bytes.truncate(40);
        write_atomic(&path, &bytes, MODE_FILE).unwrap();
        assert!(read_section(&path, ".upd_info").is_none());
    }
}
