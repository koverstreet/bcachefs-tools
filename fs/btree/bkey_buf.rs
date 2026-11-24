// SPDX-License-Identifier: GPL-2.0

//! An owned copy of a key, as C's struct bkey_buf: a small key is kept in the
//! struct, a bigger one on the heap.
//!
//! The C version points bkey_buf.k at whichever buffer is in use - into
//! itself, while the key is small - so it can't be moved. Here it's a DstRoom
//! over an InlineOrBoxed, which knows which it's in, and a BkeyBuf moves like
//! any other value.
//!
//! As in C, copying a key in can't fail: the heap buffer is allocated
//! __GFP_NOFAIL, by InlineOrBoxed's Storage::grow(). Zeroed, so the whole
//! buffer is initialized memory. And as C's, it's room for any key: a key
//! that's outgrown the struct moves once, and never again - a data update's
//! is read under RCU, by the in-flight listing, while it's in use.

use crate::btree::bkey::{BkeyI, BkeySC};
use crate::c;
use crate::format::Btree;
use core::mem::size_of;
use crate::data_structures::dst_room::DstRoom;
use crate::data_structures::dst_room::InlineOrBoxed;
use crate::util::vstructs::{plain_bytes, plain_bytes_mut};

const ONSTACK_U64S: usize = 12;

/// A key's u64s, a u8, counts its header: room for 256 holds any key - as
/// C's kmalloc(2048).
const HEAP_U64S: usize = 256;

pub type BkeyBuf = DstRoom<BkeyI, InlineOrBoxed<u64, ONSTACK_U64S, HEAP_U64S>>;

impl DstRoom<BkeyI, InlineOrBoxed<u64, ONSTACK_U64S, HEAP_U64S>> {
    /// Holding a deleted key, as bch2_bkey_buf_init() leaves it. Out of line:
    /// it zeroes the whole room, and no hot path makes one.
    #[inline(never)]
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of @k.
    pub fn from_key(k: BkeySC<'_>) -> Self {
        let mut b = Self::new();
        b.set_key(k);
        b
    }
}

/// Copies a key - header @k, value @val - to the front of @dst.
pub(crate) fn copy_key(dst: &mut [u64], k: &c::bkey, val: &[u8]) {
    let dst = plain_bytes_mut(dst);
    dst[..size_of::<c::bkey>()].copy_from_slice(plain_bytes(k));
    dst[size_of::<c::bkey>()..][..val.len()].copy_from_slice(val);
}

impl<'a> From<&'a BkeyBuf> for BkeySC<'a> {
    fn from(b: &'a BkeyBuf) -> Self {
        BkeySC::from(b.as_ref())
    }
}

impl AsRef<BkeyI> for BkeyBuf {
    fn as_ref(&self) -> &BkeyI {
        self
    }
}

impl AsMut<BkeyI> for BkeyBuf {
    fn as_mut(&mut self) -> &mut BkeyI {
        self
    }
}


/// An owned copy of a key, and where it lives: its btree, and its level - 0
/// for a leaf's key, a node pointer's the level above its node's.
pub struct BtreeKeyBuf {
    pub btree: Btree,
    pub level: u32,
    pub k:     BkeyBuf,
}

impl BtreeKeyBuf {
    pub fn new(btree: Btree, level: u32, k: BkeySC<'_>) -> Self {
        BtreeKeyBuf { btree, level, k: BkeyBuf::from_key(k) }
    }

    pub fn key(&self) -> BkeySC<'_> {
        (&self.k).into()
    }
}
