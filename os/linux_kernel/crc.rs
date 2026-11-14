// SPDX-License-Identifier: GPL-2.0

//! The crcs checksums and string hashes are made with: @crc the value so far,
//! not inverted before or after.

use crate::c;

pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    unsafe { c::crc32c(crc, data.as_ptr().cast(), data.len()) }
}

/// ECMA-182, most significant bit first.
pub fn crc64_be(crc: u64, data: &[u8]) -> u64 {
    unsafe { c::crc64_be(crc, data.as_ptr().cast(), data.len()) }
}
