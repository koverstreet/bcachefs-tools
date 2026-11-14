// SPDX-License-Identifier: GPL-2.0

//! The crcs checksums and string hashes are made with: @crc the value so far,
//! not inverted before or after, as the kernel's lib/crc.

/// The crate's is the standard CRC-32C, which inverts before and after.
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    !::crc32c::crc32c_append(!crc, data)
}

/// ECMA-182, most significant bit first.
pub fn crc64_be(crc: u64, data: &[u8]) -> u64 {
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
}
