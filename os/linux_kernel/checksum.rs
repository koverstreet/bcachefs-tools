// SPDX-License-Identifier: GPL-2.0

//! The crcs checksums and string hashes are made with: @crc the value so far,
//! not inverted before or after.

#[path = "../checksum.rs"]
mod common;
pub use common::*;

use crate::c;

fn crc32c(crc: u32, data: &[u8]) -> u32 {
    unsafe { c::crc32c(crc, data.as_ptr().cast(), data.len()) }
}

/// ECMA-182, most significant bit first.
fn crc64_be(crc: u64, data: &[u8]) -> u64 {
    unsafe { c::crc64_be(crc, data.as_ptr().cast(), data.len()) }
}
