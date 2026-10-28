// SPDX-License-Identifier: GPL-2.0

//! Rust's part of reading btree nodes (btree/read.c): so far, a bset's
//! checksum and encryption - its nonce, and a node's and a node entry's
//! checksum() and encrypt().

use crate::c;
use crate::data::checksum::{csum_vstruct, BCH_NONCE_BTREE, CHACHA_BLOCK_SIZE};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::types::Le32;
use crate::util::vstructs::{Head, Unvalidated};
use core::mem::offset_of;

impl c::bset__head {
    /// The nonce of this bset, at byte @offset of its btree node: the offset,
    /// the bset's seq, and the low half of its journal seq, in the btree
    /// domain.
    pub fn nonce(&self, offset: u32) -> c::nonce {
        let seq = self.seq.get();
        c::nonce {
            d: [
                Le32::new(offset),
                Le32::new(seq as u32),
                Le32::new((seq >> 32) as u32),
                Le32::new(self.journal_seq.get() as u32 ^ BCH_NONCE_BTREE),
            ],
        }
    }

    fn checksum_type(&self) -> c::bch_csum_type {
        c::bch_csum_type(self.csum_type() as u32)
    }
}

/// Where a bset's keys start in a node, or a node entry: its tail, past its
/// header.
const fn keys_offset(bset: usize) -> usize {
    bset + size_of::<Head<c::bset>>()
}

/// A node's first bset - its checksum covers the node from past its csum,
/// and its encryption the node's header from flags on too, ahead of the keys,
/// their nonce starting at the next ChaCha20 block after it.
impl c::btree_node<Unvalidated> {
    pub fn checksum(&self, fs: &Fs) -> c::bch_csum {
        csum_vstruct(Some(fs), self.keys.checksum_type(), self.keys.nonce(0), self)
    }

    /// Or decrypted: ChaCha20 is its own inverse.
    pub fn encrypt(&mut self, fs: &Fs) -> Result<(), BchError> {
        let (ty, nonce) = (self.keys.checksum_type(), self.keys.nonce(0));
        let (flags, keys) = (offset_of!(Head<c::btree_node>, flags), offset_of!(Head<c::btree_node>, keys));
        let bytes = self.as_bytes_mut();

        fs.encrypt(ty, nonce, &mut bytes[flags..keys])?;
        let nonce = nonce + ((keys - flags) as u32).next_multiple_of(CHACHA_BLOCK_SIZE);
        fs.encrypt(ty, nonce, &mut bytes[keys_offset(keys)..])
    }
}

/// A later bset, at byte @offset of its node.
impl c::btree_node_entry<Unvalidated> {
    pub fn checksum(&self, fs: &Fs, offset: u32) -> c::bch_csum {
        csum_vstruct(Some(fs), self.keys.checksum_type(), self.keys.nonce(offset), self)
    }

    /// Or decrypted, as a node's.
    pub fn encrypt(&mut self, fs: &Fs, offset: u32) -> Result<(), BchError> {
        let (ty, nonce) = (self.keys.checksum_type(), self.keys.nonce(offset));
        let keys = keys_offset(offset_of!(Head<c::btree_node_entry>, keys));
        fs.encrypt(ty, nonce, &mut self.as_bytes_mut()[keys..])
    }
}
