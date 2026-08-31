use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const CLASS_64: u8 = 2;
const DATA_LITTLE_ENDIAN: u8 = 1;
const SECTION_HEADER_SIZE: usize = 64;

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
    fn truncated_files_do_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.elf");
        let mut bytes = elf_with_section(".upd_info", b"data");
        bytes.truncate(40);
        write_atomic(&path, &bytes, MODE_FILE).unwrap();
        assert!(read_section(&path, ".upd_info").is_none());
    }
}
