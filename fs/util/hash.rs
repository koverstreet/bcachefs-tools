// SPDX-License-Identifier: GPL-2.0

//! linux/hash.h's hash_64(), for Rust: the golden ratio multiply, its top
//! bits the hash - and Fold, a core::hash::Hasher that folds a key's words
//! into the one word it multiplies, so any Hash key has a hash_64().
//!
//! Cheap - a multiply a word - and well spread for keys counting up, but
//! not keyed: for tables whose keys an attacker doesn't choose.

use crate::c;
use core::hash::{Hash, Hasher};

/// The top @bits of @v times the golden ratio. Zero @bits: all of it.
pub fn hash_64(v: u64, bits: u32) -> u64 {
    v.wrapping_mul(c::GOLDEN_RATIO_64).wrapping_shr(64 - bits)
}

/// A key's words folded into one, by hash_64()'s multiply: a u64 key's is
/// itself.
#[derive(Default)]
pub struct Fold(u64);

impl Fold {
    /// @key, folded: what hash_64() takes.
    pub fn of<K: ?Sized + Hash>(key: &K) -> u64 {
        let mut f = Fold::default();
        key.hash(&mut f);
        f.finish()
    }
}

impl Hasher for Fold {
    fn write(&mut self, bytes: &[u8]) {
        for w in bytes.chunks(8) {
            let mut b = [0; 8];
            b[..w.len()].copy_from_slice(w);
            self.write_u64(u64::from_le_bytes(b));
        }
    }

    fn write_u64(&mut self, w: u64) {
        self.0 = self.0.wrapping_mul(c::GOLDEN_RATIO_64) ^ w;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
