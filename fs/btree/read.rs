// SPDX-License-Identifier: GPL-2.0

//! Rust's part of reading btree nodes (btree/read.c): so far, a bset's
//! encryption - its nonce, and bset_encrypt().

use crate::c;
use crate::data::checksum::{BCH_NONCE_BTREE, CHACHA_BLOCK_SIZE};
use crate::errcode::{ret_to_result_void, BchError};
use core::mem::offset_of;
use zerocopy::byteorder::little_endian::U32;

impl c::bset {
    /// The nonce of this bset, at byte @offset of its btree node: the offset,
    /// the bset's seq, and the low half of its journal seq, in the btree
    /// domain.
    pub fn nonce(&self, offset: u32) -> c::nonce {
        let seq = self.seq.get();
        c::nonce {
            d: [
                U32::new(offset),
                U32::new(seq as u32),
                U32::new((seq >> 32) as u32),
                U32::new(self.journal_seq.get() as u32 ^ BCH_NONCE_BTREE),
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
pub unsafe fn bset_encrypt(c: *mut c::bch_fs, i: *mut c::bset, offset: u32) -> Result<(), BchError> {
    let csum_type = (*i).csum_type() as u32;
    let mut nonce = (*i).nonce(offset);

    if offset == 0 {
        let bn = i.byte_sub(offset_of!(c::btree_node, bset)).cast::<c::btree_node>();
        let start = offset_of!(c::btree_node, flags);
        let bytes = offset_of!(c::btree_node, bset) - start;

        ret_to_result_void(c::bch2_encrypt(c, csum_type, nonce, bn.byte_add(start).cast(), bytes))?;
        nonce = nonce.add((bytes as u32).next_multiple_of(CHACHA_BLOCK_SIZE));
    }

    ret_to_result_void(c::bch2_encrypt(c, csum_type, nonce,
                                       i.byte_add(offset_of!(c::bset, _data)).cast(),
                                       (*i).u64s.get() as usize * 8))
}
