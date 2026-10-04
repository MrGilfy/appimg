//! The zip and tar.gz archives the tests install and adopt from, built by
//! hand, the way a release ships them.

#![allow(dead_code)]

use std::io::Write;

use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compression, Crc};

/// A zip archive of deflated files, as PKWARE's APPNOTE lays one out.
pub fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data).unwrap();
        let packed = encoder.finish().unwrap();
        let mut crc = Crc::new();
        crc.update(data);
        let offset = out.len() as u32;
        let common = |out: &mut Vec<u8>| {
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&8u16.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&crc.sum().to_le_bytes());
            out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
        };
        out.extend_from_slice(b"PK\x03\x04");
        common(&mut out);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&packed);
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&(3u16 << 8 | 20).to_le_bytes());
        common(&mut central);
        // Comment length, disk, internal and external attributes.
        central.extend_from_slice(&[0; 10]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let directory_at = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&directory_at.to_le_bytes());
    out.extend_from_slice(&[0; 2]);
    out
}

/// A gzip-compressed tar file of regular files, in ustar form.
pub fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = Vec::new();
    for (name, data) in files {
        let mut header = vec![0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..265].copy_from_slice(b"ustar\x0000");
        let sum: u32 = header.iter().map(|byte| *byte as u32).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        tar.extend(header);
        tar.extend_from_slice(data);
        tar.resize(tar.len().next_multiple_of(512), 0);
    }
    tar.extend([0u8; 1024]);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap()
}
