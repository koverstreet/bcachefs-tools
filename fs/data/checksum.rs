// SPDX-License-Identifier: GPL-2.0

//! Nonces: ChaCha20's, for encrypted metadata and data. They're on-disk
//! format - decrypting takes the nonce the data was encrypted with.
//!
//! A nonce is four little-endian words. The top bits of the last are its
//! domain, what's encrypted with it, so that btree nodes, journal entries and
//! extents never share one; the first counts ChaCha20 blocks, which add()
//! advances. A domain's nonce is made where its structure is: bset::nonce()
//! in btree/read.rs, jset::nonce() in journal/read.rs.
//!
//! C has its own in data/checksum.h: BCH_NONCE_*, nonce_add().

use crate::c;

/// ChaCha20's block, in bytes: what a nonce's first word counts.
pub const CHACHA_BLOCK_SIZE: u32 = 64;

/// The domains, in a nonce's last word.
pub const BCH_NONCE_EXTENT:	u32 = 1 << 28;
pub const BCH_NONCE_BTREE:	u32 = 2 << 28;
pub const BCH_NONCE_JOURNAL:	u32 = 3 << 28;
pub const BCH_NONCE_PRIO:	u32 = 4 << 28;
pub const BCH_NONCE_POLY:	u32 = 1 << 31;

impl c::nonce {
    /// The nonce @offset bytes further into its stream - a whole number of
    /// ChaCha20 blocks.
    pub fn add(mut self, offset: u32) -> Self {
        debug_assert!(offset % CHACHA_BLOCK_SIZE == 0,
                      "nonce offset {offset}: not a whole number of ChaCha20 blocks");
        self.d[0] = u32::from_le(self.d[0]).wrapping_add(offset / CHACHA_BLOCK_SIZE).to_le();
        self
    }
}
