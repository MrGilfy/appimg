//! An AppImage that tells whether anything ran it: run, it creates the file
//! it names and does nothing else. The shell script the adopt scan test
//! leaves a mark with cannot get this far, every install and adoption checks
//! for an ELF header before it reads the metadata, so this one is a real
//! ELF executable. Behind the program come a `payload=` line for the
//! stand-in for `unsquashfs` and the squashfs magic that tells appimg where
//! the payload starts, the way the other fixtures end.

#![allow(dead_code)]

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Where the program starts: behind the 64 bytes of the ELF header and the
/// 56 of the one program header, which loads the whole file.
const CODE_OFFSET: u64 = 120;
const LOAD_ADDRESS: u64 = 0x40_0000;

/// `creat(mark, 0644)`, then `exit(0)`, with the path of the mark right
/// behind the code. Returns the ELF machine and the code.
#[cfg(target_arch = "x86_64")]
fn program() -> (u16, Vec<u8>) {
    let code = [
        0x48, 0x8d, 0x3d, 0x15, 0x00, 0x00, 0x00, // lea rdi, [rip + 21]: the mark
        0xbe, 0xa4, 0x01, 0x00, 0x00, // mov esi, 0o644
        0xb8, 0x55, 0x00, 0x00, 0x00, // mov eax, 85: creat
        0x0f, 0x05, // syscall
        0xb8, 0x3c, 0x00, 0x00, 0x00, // mov eax, 60: exit
        0x31, 0xff, // xor edi, edi
        0x0f, 0x05, // syscall
    ];
    (62, code.to_vec())
}

/// `openat(AT_FDCWD, mark, O_WRONLY | O_CREAT | O_TRUNC, 0644)`, then
/// `exit(0)`, with the path of the mark right behind the code.
#[cfg(target_arch = "aarch64")]
fn program() -> (u16, Vec<u8>) {
    let code: [u32; 9] = [
        0x9280_0c60, // mov x0, #-100: AT_FDCWD
        0x1000_0101, // adr x1, #32: the mark
        0xd280_4822, // mov x2, #0x241
        0xd280_3483, // mov x3, #0o644
        0xd280_0708, // mov x8, #56: openat
        0xd400_0001, // svc #0
        0xd280_0000, // mov x0, #0
        0xd280_0ba8, // mov x8, #93: exit
        0xd400_0001, // svc #0
    ];
    (183, code.iter().flat_map(|word| word.to_le_bytes()).collect())
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn program() -> (u16, Vec<u8>) {
    panic!("the marker AppImage has a program for x86_64 and aarch64 only")
}

/// Writes the marker AppImage to `path` with `mode`. Run, it creates
/// `mark`. Unpacked by the stand-in for `unsquashfs`, it is `payload`.
pub fn write(path: &Path, mark: &Path, payload: &Path, mode: u32) {
    let (machine, code) = program();
    let mut tail = mark.as_os_str().as_bytes().to_vec();
    tail.push(0);
    tail.extend_from_slice(format!("\npayload={}\nhsqs\n", payload.display()).as_bytes());
    let size = CODE_OFFSET + code.len() as u64 + tail.len() as u64;

    // The ELF magic, 64-bit, little-endian, version 1, then the AppImage
    // magic for type 2 where the ELF header leaves room for it.
    let mut bytes = b"\x7fELF\x02\x01\x01\x00AI\x02\x00\x00\x00\x00\x00".to_vec();
    bytes.extend_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    bytes.extend_from_slice(&machine.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes()); // version
    bytes.extend_from_slice(&(LOAD_ADDRESS + CODE_OFFSET).to_le_bytes()); // entry
    bytes.extend_from_slice(&64u64.to_le_bytes()); // program headers
    bytes.extend_from_slice(&0u64.to_le_bytes()); // no section headers
    bytes.extend_from_slice(&0u32.to_le_bytes()); // flags
                                                  // Header size, program header size and count, section header size,
                                                  // count and string table index.
    for half in [64u16, 56, 1, 0, 0, 0] {
        bytes.extend_from_slice(&half.to_le_bytes());
    }
    // PT_LOAD, readable and executable: offset, virtual and physical
    // address, size in the file and in memory, alignment.
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&5u32.to_le_bytes());
    for word in [0, LOAD_ADDRESS, LOAD_ADDRESS, size, size, 0x1000] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    assert_eq!(bytes.len() as u64, CODE_OFFSET);
    bytes.extend_from_slice(&code);
    bytes.extend_from_slice(&tail);

    // Only complete and closed under its final name: executing a file that
    // any process still holds open for writing fails with `ETXTBSY`.
    let partial = path.with_extension("partial");
    let mut file = File::create(&partial).unwrap();
    file.write_all(&bytes).unwrap();
    drop(file);
    fs::set_permissions(&partial, fs::Permissions::from_mode(mode)).unwrap();
    fs::rename(&partial, path).unwrap();
}
