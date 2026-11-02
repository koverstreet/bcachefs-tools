// SPDX-License-Identifier: GPL-2.0

//! Rust's part of reading btree nodes (btree/read.c): so far, a bset's
//! checksum and encryption - its nonce, and a node's and a node entry's
//! checksum() and encrypt().

#![allow(non_camel_case_types)]

use crate::fs::Dev;
use crate::btree::bkey::BkeyI;
use crate::btree::node::BtreeNode;
use crate::c;
use crate::data::checksum::{csum_vstruct, BCH_NONCE_BTREE, CHACHA_BLOCK_SIZE};
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::types::{Le32, c_default};
use crate::util::vstructs::{Head, Unvalidated};
use core::mem::offset_of;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::{CStruct, bitfield, c_extern, c_typedef};
use nestify::nest;
use crate::format::{Btree, Nonce};

nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct btree_read_bio {
        pub c: *mut Fs,
        pub ca: *mut Dev, /* stashed at submit; see bch_write_bio */
        pub b: *mut BtreeNode,
        pub start_time: u64,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct btree_read_bio_bits {
            idx:   u7,
            __pad: u1,
        },
        /* its slot on its async object list */
        #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
        pub list_idx: c_uint,
        pub pick: c::extent_ptr_decoded,
        pub work: c::work_struct,
        pub bio: c::bio,
    }
}
c_default!(btree_read_bio);
crate::has_bio!(btree_read_bio, bio);

c_typedef! {
    pub type btree_node_scrub_report_fn = Option<unsafe extern "C" fn(priv_: *mut c_void,
                                                                     dev: c_uint, good: bool)>;
}

c_extern! {
    pub fn bch2_btree_node_scrub(trans: *mut c::btree_trans, btree: Btree, level: c_uint,
                                 k: c::bkey_s_c, dev: c_uint) -> c_int;
    pub fn bch2_btree_flush_all_reads(c: *mut Fs) -> bool;
    pub fn bch2_btree_root_read(c: *mut Fs, btree: Btree, k: *const c::bkey_i, level: c_uint) -> c_int;
}

impl Fs {
    pub fn btree_root_read(&self, btree: Btree, k: &BkeyI, level: u32) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_btree_root_read(self.as_ptr(), btree, k.as_ffi(), level) })
    }
}

impl c::bset__head {
    /// The nonce of this bset, at byte @offset of its btree node: the offset,
    /// the bset's seq, and the low half of its journal seq, in the btree
    /// domain.
    pub fn nonce(&self, offset: u32) -> Nonce {
        let seq = self.seq.get();
        Nonce {
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
