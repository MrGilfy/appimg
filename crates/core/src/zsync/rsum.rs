//! The weak, rolling checksum zsync slides over a local file.

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
    /// Read by the scan, which mixes both halves into its lookup key.
    pub(super) a: u16,
    pub(super) b: u16,
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

#[cfg(test)]
mod tests {
    use super::super::control::tests::control_bytes;
    use super::super::control::{parse_control, HashLengths};
    use super::*;

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
}
