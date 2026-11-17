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

pub struct Xxh64(c::xxh64_state);

impl Checksum for Xxh64 {
    type Seed = u64;
    type Output = u64;

    fn new(seed: u64) -> Self {
        let mut s = c::xxh64_state::default();
        unsafe { c::xxh64_reset(&mut s, seed) };
        Xxh64(s)
    }

    fn update(&mut self, data: &[u8]) {
        unsafe { c::xxh64_update(&mut self.0, data.as_ptr().cast(), data.len()) };
    }

    fn finalize(self) -> u64 {
        unsafe { c::xxh64_digest(&self.0) }
    }
}
