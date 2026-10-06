// SPDX-License-Identifier: GPL-2.0

//! An owned copy of a key, as C's struct bkey_buf: a small key is kept in the
//! struct, a bigger one in a heap buffer with room for any key.
//!
//! The C version points bkey_buf.k at whichever buffer is in use - into
//! itself, while the key is small - so it can't be moved. Here the buffer in
//! use is decided on each access, and a BkeyBuf moves like any other value.
//!
//! As in C, copying a key in can't fail: the heap buffer is allocated
//! __GFP_NOFAIL, through rust_kzalloc_nofail() - the kernel's Rust allocator
//! API doesn't offer it. Zeroed, so the whole buffer is initialized memory.

use crate::btree::bkey::{BkeySC, BKEY_U64S};
use crate::c;
use core::mem::size_of;
use core::ptr::NonNull;
use core::slice;

const ONSTACK_U64S: usize = 12;

/// A key's u64s, a u8, counts its header: room for 256 holds any key - as
/// C's kmalloc(2048).
const HEAP_U64S: usize = 256;

pub struct BkeyBuf {
    onstack: [u64; ONSTACK_U64S],
    heap:    Option<NonNull<u64>>,
}

impl BkeyBuf {
    /// Holding a deleted key, as bch2_bkey_buf_init() leaves it.
    pub fn new() -> Self {
        let mut b = BkeyBuf { onstack: [0; ONSTACK_U64S], heap: None };
        unsafe { c::bkey_init(&mut b.k_i_mut().k) };
        b
    }

    fn buf(&self) -> &[u64] {
        match self.heap {
            Some(p) => unsafe { slice::from_raw_parts(p.as_ptr(), HEAP_U64S) },
            None    => &self.onstack,
        }
    }

    fn buf_mut(&mut self) -> &mut [u64] {
        match self.heap {
            Some(p) => unsafe { slice::from_raw_parts_mut(p.as_ptr(), HEAP_U64S) },
            None    => &mut self.onstack,
        }
    }

    /// Make room for a key of @u64s, as bch2_bkey_buf_realloc(): once on the
    /// heap, a key stays there.
    fn realloc(&mut self, u64s: usize) {
        if self.heap.is_some() || u64s <= ONSTACK_U64S {
            return;
        }

        let p = unsafe { c::rust_kzalloc_nofail(HEAP_U64S * size_of::<u64>()) } as *mut u64;
        let p = NonNull::new(p).expect("__GFP_NOFAIL allocation returned NULL");

        unsafe { core::ptr::copy_nonoverlapping(self.onstack.as_ptr(), p.as_ptr(), ONSTACK_U64S) };
        self.heap = Some(p);
    }

    /// Copy @k in, as bch2_bkey_buf_reassemble().
    pub fn reassemble(&mut self, k: BkeySC<'_>) {
        let u64s = k.k.u64s as usize;
        self.realloc(u64s);

        let buf = self.buf_mut();
        unsafe {
            core::ptr::copy_nonoverlapping(k.k as *const c::bkey as *const u64,
                                           buf.as_mut_ptr(), BKEY_U64S);
            core::ptr::copy_nonoverlapping(k.v as *const c::bch_val as *const u64,
                                           buf[BKEY_U64S..].as_mut_ptr(), u64s - BKEY_U64S);
        }
    }

    pub fn k_i(&self) -> &c::bkey_i {
        unsafe { &*(self.buf().as_ptr() as *const c::bkey_i) }
    }

    pub fn k_i_mut(&mut self) -> &mut c::bkey_i {
        unsafe { &mut *(self.buf_mut().as_mut_ptr() as *mut c::bkey_i) }
    }

    pub fn sc(&self) -> BkeySC<'_> {
        BkeySC::from(self.k_i())
    }
}

impl Default for BkeyBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BkeyBuf {
    fn drop(&mut self) {
        if let Some(p) = self.heap {
            unsafe { c::rust_kfree(p.as_ptr() as *const core::ffi::c_void) };
        }
    }
}
