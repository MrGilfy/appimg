//! The zsync parser against a real control file, as published. The
//! hand-built cases live next to the parser; what this file proves is that
//! the format as a zsync in the wild writes it is read the way the parser
//! thinks it is.

use appimg_core::zsync::{self, HashLengths};

/// `obsolete-appimagetool-x86_64.AppImage.zsync` from release 13 of
/// AppImageKit, written by zsync 0.6.2 in 2020 and unchanged since. Six and a
/// half kilobytes describing an AppImage of 2172096 bytes: small enough to
/// keep in the repository, and not edited in any way.
const CONTROL: &[u8] = include_bytes!("fixtures/appimagetool-x86_64.appimage.zsync");

/// Where the header ends. The blank line is at 224, the table starts at 226.
const TABLE_AT: usize = 226;

#[test]
fn reads_a_control_file_as_published() {
    let control = zsync::parse_control(CONTROL).unwrap();

    assert_eq!(control.header.filename.as_deref(), Some("appimagetool-x86_64.AppImage"));
    assert_eq!(control.header.length, 2_172_096);
    assert_eq!(control.header.sha1.as_deref(), Some("3c7d3061b7f2372d314fee553f3e37d2c2e5c03b"));
    assert_eq!(control.header.url.as_deref(), Some("appimagetool-x86_64.AppImage"));
    assert_eq!(control.blocksize, 2048);
    assert_eq!(
        control.hash_lengths,
        HashLengths { seq_matches: 2, rsum_bytes: 2, checksum_bytes: 4 }
    );

    // 2172096 bytes in blocks of 2048 divides evenly.
    assert_eq!(control.blocks.len(), 1061);
    assert_eq!(
        control.blocks.len() * control.hash_lengths.entry_size(),
        CONTROL.len() - TABLE_AT,
        "the table has to be the whole file after the header"
    );
}

#[test]
fn reads_the_first_and_the_last_entry_of_the_table() {
    let control = zsync::parse_control(CONTROL).unwrap();
    let last = control.blocks.len() - 1;

    assert_eq!(control.blocks[0].rsum, 0x0000_1e24);
    assert_eq!(control.checksum(0).unwrap(), &[0xca, 0x43, 0xb5, 0x16]);
    assert_eq!(control.blocks[1].rsum, 0x0000_d8aa);
    assert_eq!(control.checksum(1).unwrap(), &[0x18, 0x94, 0x03, 0x7f]);
    // The last block of this file is all zeroes, so its rolling checksum is
    // zero as well. The strong checksum still tells it apart.
    assert_eq!(control.blocks[last].rsum, 0x0000_0000);
    assert_eq!(control.checksum(last).unwrap(), &[0xe7, 0xb9, 0xe6, 0xb0]);
    assert_eq!(control.checksum(last + 1), None);
}

#[test]
fn the_header_alone_still_reads_out_of_the_first_kilobytes() {
    // What a check does: one ranged request, no table in hand.
    let header = zsync::parse_header(&CONTROL[..TABLE_AT + 64]).unwrap();
    assert_eq!(header.length, 2_172_096);
    assert_eq!(header.blocksize, Some(2048));
    assert_eq!(header.hash_lengths.checksum_bytes, 4);
}

#[test]
fn a_truncated_control_file_is_an_error_not_a_short_table() {
    // A download that stopped, at every size a request might have returned.
    for cut in [TABLE_AT, 1024, CONTROL.len() / 2, CONTROL.len() - 1] {
        let reason = zsync::parse_control(&CONTROL[..cut]).unwrap_err();
        assert!(reason.contains("short"), "{cut}: {reason}");
    }
}

#[test]
fn a_corrupt_control_file_is_refused() {
    // A header that lost its blank line: everything is one long header.
    let mut broken = CONTROL.to_vec();
    broken[TABLE_AT - 2] = b'X';
    assert!(zsync::parse_control(&broken).is_err());

    // A length that no longer matches the table it came with.
    let text = String::from_utf8_lossy(&CONTROL[..TABLE_AT]).replace("2172096", "9172096");
    let mut broken = text.into_bytes();
    broken.extend_from_slice(&CONTROL[TABLE_AT..]);
    assert!(zsync::parse_control(&broken).is_err());
}
