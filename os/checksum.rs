// SPDX-License-Identifier: GPL-2.0

/// A checksum computed over its data's pieces in order, from a seed.
pub trait Checksum: Sized {
    type Seed;
    type Output;

    fn new(seed: Self::Seed) -> Self;

    fn update(&mut self, data: &[u8]);

    fn chain_update(mut self, data: &[u8]) -> Self {
        self.update(data);
        self
    }

    fn finalize(self) -> Self::Output;
}

pub struct Crc32c(u32);

impl Checksum for Crc32c {
    type Seed = u32;
    type Output = u32;

    fn new(seed: u32) -> Self {
        Crc32c(seed)
    }

    fn update(&mut self, data: &[u8]) {
        self.0 = super::crc32c(self.0, data);
    }

    fn finalize(self) -> u32 {
        self.0
    }
}

pub struct Crc64(u64);

impl Checksum for Crc64 {
    type Seed = u64;
    type Output = u64;

    fn new(seed: u64) -> Self {
        Crc64(seed)
    }

    fn update(&mut self, data: &[u8]) {
        self.0 = super::crc64_be(self.0, data);
    }

    fn finalize(self) -> u64 {
        self.0
    }
}
