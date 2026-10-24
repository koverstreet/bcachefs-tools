// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2013 Andre Oppermann <andre@FreeBSD.org>

//! SipHash-c-d, Aumasson and Bernstein's keyed hash ("SipHash: a fast
//! short-input PRF"): c compression rounds per 8 byte block, d finalization
//! rounds, a 128 bit key and a 64 bit result. The round counts are the type's:
//! str_hash's SipHash24, the full strength, and SipHash13 - hsiphash()'s
//! rounds on 64-bit - for hash tables, util/cuckoo.rs's.
//!
//! Translated from util/siphash.c, OpenBSD's siphash.c 1.3, which it replaces -
//! and like it, not quite SipHash when one input comes in several updates. An
//! update that finishes a part filled block copies what's left after it to
//! where the block was filled to, not to its start, and the end then zeroes
//! from there: so the last block hashes stale bytes and loses some of the tail.
//! xattr names hash as two updates, their type and then the name, so an xattr
//! name of seven bytes or more hashes this way on disk, unless what it has past
//! the seventh byte is a multiple of eight. The result is the format: kept as
//! is, and checked against util/siphash.c before that went. One update hashes
//! as SipHash does - every dirent, and the cuckoo table's. A tail that doesn't
//! fit where it's put - C wrote it into the byte count - is a bounds check
//! here: the one hash in two updates, an xattr's, puts one byte in the block
//! first, which leaves room for any tail.
//!
//! As a core::hash::Hasher - what a hash table's key is hashed into, a write
//! per field - it's SipHash proper, whatever the writes: a key whose fields
//! lost bytes to the above would hash alike under every key.

/// SipHash with @C compression and @D finalization rounds, as it runs.
#[derive(Clone)]
pub struct SipHasher<const C: usize, const D: usize> {
    v:     [u64; 4],
    buf:   [u8; 8],
    bytes: u32,
}

pub type SipHash24 = SipHasher<2, 4>;
pub type SipHash13 = SipHasher<1, 3>;

/// A SipHash key, as a core::hash::BuildHasher: what a keyed table hashes
/// with, hash_one() - drawn when the table's made, and again on a rehash.
#[derive(Clone, Copy)]
pub struct SipKey<const C: usize, const D: usize>([u64; 2]);

pub type SipHash13Key = SipKey<1, 3>;

impl<const C: usize, const D: usize> From<[u64; 2]> for SipKey<C, D> {
    fn from(k: [u64; 2]) -> Self {
        Self(k)
    }
}

impl<const C: usize, const D: usize> core::hash::BuildHasher for SipKey<C, D> {
    type Hasher = SipHasher<C, D>;

    fn build_hasher(&self) -> SipHasher<C, D> {
        SipHasher::new(self.0[0], self.0[1])
    }
}

impl<const C: usize, const D: usize> SipHasher<C, D> {
    pub fn new(k0: u64, k1: u64) -> Self {
        Self {
            v: [0x736f6d6570736575 ^ k0, 0x646f72616e646f6d ^ k1,
                0x6c7967656e657261 ^ k0, 0x7465646279746573 ^ k1],
            buf:   [0; 8],
            bytes: 0,
        }
    }

    fn rounds(&mut self, n: usize) {
        let [mut v0, mut v1, mut v2, mut v3] = self.v;
        for _ in 0..n {
            v0 = v0.wrapping_add(v1);
            v2 = v2.wrapping_add(v3);
            v1 = v1.rotate_left(13);
            v3 = v3.rotate_left(16);

            v1 ^= v0;
            v3 ^= v2;
            v0 = v0.rotate_left(32);

            v2 = v2.wrapping_add(v1);
            v0 = v0.wrapping_add(v3);
            v1 = v1.rotate_left(17);
            v3 = v3.rotate_left(21);

            v1 ^= v2;
            v3 ^= v0;
            v2 = v2.rotate_left(32);
        }
        self.v = [v0, v1, v2, v3];
    }

    fn block(&mut self, m: u64) {
        self.v[3] ^= m;
        self.rounds(C);
        self.v[0] ^= m;
    }

    /// @data, as the format hashes it: see the top of the file.
    pub fn update(&mut self, data: &[u8]) {
        self.absorb(data, false)
    }

    /// @data; the tail after a block @data finished goes where the format puts
    /// it, past where that block had been filled to, unless @exact.
    fn absorb(&mut self, mut data: &[u8], exact: bool) {
        if data.is_empty() {
            return;
        }

        let used = self.bytes as usize % 8;
        self.bytes = self.bytes.wrapping_add(data.len() as u32);
        let mut at = used;

        if used > 0 {
            let left = 8 - used;
            if data.len() < left {
                self.buf[used..used + data.len()].copy_from_slice(data);
                return;
            }
            self.buf[used..].copy_from_slice(&data[..left]);
            self.block(u64::from_le_bytes(self.buf));
            data = &data[left..];
            if exact {
                at = 0;
            }
        }

        let mut blocks = data.chunks_exact(8);
        for b in &mut blocks {
            self.block(u64::from_le_bytes(b.try_into().expect("8 bytes")));
        }
        let tail = blocks.remainder();
        self.buf[at..at + tail.len()].copy_from_slice(tail);
    }

    pub fn end(mut self) -> u64 {
        let used = self.bytes as usize % 8;
        self.buf[used..7].fill(0);
        self.buf[7] = self.bytes as u8;
        self.block(u64::from_le_bytes(self.buf));

        self.v[2] ^= 0xff;
        self.rounds(D);
        self.v.into_iter().fold(0, |h, v| h ^ v)
    }

    /// @data's hash, in one update.
    pub fn hash(k0: u64, k1: u64, data: &[u8]) -> u64 {
        let mut h = Self::new(k0, k1);
        h.update(data);
        h.end()
    }
}

impl<const C: usize, const D: usize> core::hash::Hasher for SipHasher<C, D> {
    fn write(&mut self, bytes: &[u8]) {
        self.absorb(bytes, true)
    }

    fn finish(&self) -> u64 {
        self.clone().end()
    }
}
