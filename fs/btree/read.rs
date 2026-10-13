// SPDX-License-Identifier: GPL-2.0

//! Rust's part of reading btree nodes (btree/read.c): so far, a bset's
//! encryption - its nonce, and bset_encrypt().

use crate::c;
use crate::data::checksum::{BCH_NONCE_BTREE, CHACHA_BLOCK_SIZE};
use crate::errcode::{ret_to_result_void, BchError};
use crate::types::Le32;
use crate::util::vstructs::Ffi;
use core::mem::offset_of;

impl c::bset__ffi {
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
}

/// Encrypts bset @i, at byte @offset of its btree node - or decrypts it, as
/// ChaCha20 is its own inverse. The keys are encrypted; in a node's first
/// bset, offset 0, so is the btree_node header from flags to the bset, ahead
/// of them, the keys' nonce starting at the next block after it.
///
/// # Safety
/// @i points to a bset and the u64s of keys it says it has, at byte @offset
/// of a btree node: at offset 0, the bset of a struct btree_node.
pub unsafe fn bset_encrypt(c: *mut c::bch_fs, i: *mut Ffi<c::bset>, offset: u32) -> Result<(), BchError> {
    let csum_type = (*i).csum_type() as u32;
    let mut nonce = (*i).nonce(offset);

    if offset == 0 {
        let keys = offset_of!(Ffi<c::btree_node>, keys);
        let bn = i.byte_sub(keys).cast::<Ffi<c::btree_node>>();
        let start = offset_of!(Ffi<c::btree_node>, flags);
        let bytes = keys - start;

        ret_to_result_void(c::bch2_encrypt(c, csum_type, nonce, bn.byte_add(start).cast(), bytes))?;
        nonce = nonce.add((bytes as u32).next_multiple_of(CHACHA_BLOCK_SIZE));
    }

    // its keys: its tail, past its header
    ret_to_result_void(c::bch2_encrypt(c, csum_type, nonce,
                                       i.byte_add(size_of::<Ffi<c::bset>>()).cast(),
                                       (*i).u64s() * 8))
}
