// SPDX-License-Identifier: GPL-2.0

//! The crcs checksums and string hashes are made with: @crc the value so far,
//! not inverted before or after, as the kernel's lib/crc.

#[path = "../checksum.rs"]
mod common;
pub use common::*;

/// The crate's is the standard CRC-32C, which inverts before and after.
fn crc32c(crc: u32, data: &[u8]) -> u32 {
    !::crc32c::crc32c_append(!crc, data)
}

/// ECMA-182, most significant bit first.
fn crc64_be(crc: u64, data: &[u8]) -> u64 {
    const TABLE: [u64; 256] = {
        let mut t = [0u64; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = (i as u64) << 56;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc >> 63 != 0 { (crc << 1) ^ 0x42f0_e1eb_a9ea_3693 } else { crc << 1 };
                bit += 1;
            }
            t[i] = crc;
            i += 1;
        }
        t
    };
    data.iter().fold(crc, |crc, &b| TABLE[((crc >> 56) as u8 ^ b) as usize] ^ (crc << 8))
}

pub struct Xxh64(twox_hash::XxHash64);

impl Checksum for Xxh64 {
    type Seed = u64;
    type Output = u64;

    fn new(seed: u64) -> Self {
        Xxh64(twox_hash::XxHash64::with_seed(seed))
    }

    fn update(&mut self, data: &[u8]) {
        core::hash::Hasher::write(&mut self.0, data)
    }

    fn finalize(self) -> u64 {
        core::hash::Hasher::finish(&self.0)
    }
}

#[cfg(test)]
mod tests {
    // CRC-32C's check value, as the kernel's crc32c() is called for it:
    // seeded and finished with ~0
    #[test]
    fn crc32c() {
        assert_eq!(!super::crc32c(!0, b"123456789"), 0xe306_9283);
    }

    // CRC-64/ECMA-182's check value: what lib/crc64's crc64_be() gives
    #[test]
    fn crc64_be() {
        assert_eq!(super::crc64_be(0, b"123456789"), 0x6c40_df5f_0b49_7347);
    }

    // XXH64's, as the C lib/xxhash it replaced gave: of nothing, and of 100
    // bytes under a seed, given in pieces
    #[test]
    fn xxh64() {
        use super::{Checksum, Xxh64};

        assert_eq!(Xxh64::new(0).finalize(), 0xef46_db37_51d8_e999);

        let data: [u8; 100] = core::array::from_fn(|i| i as u8);
        let h = Xxh64::new(12345).chain_update(&data[..3]).chain_update(&data[3..43]).chain_update(&data[43..]);
        assert_eq!(h.finalize(), 0x028b_a1ae_2de4_de27);
    }
}
