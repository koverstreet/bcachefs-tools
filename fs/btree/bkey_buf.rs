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

use crate::btree::bkey::{BkeyI, BkeySC, BKEY_U64S};
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
        b.as_mut().k = c::bkey::new();
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

    /// A copy of @k.
    pub fn from_key(k: BkeySC<'_>) -> Self {
        let mut b = Self::new();
        b.reassemble(k);
        b
    }

    /// Copy @k in, as bch2_bkey_buf_reassemble().
    pub fn reassemble(&mut self, k: BkeySC<'_>) {
        self.realloc(k.k.u64s as usize);
        // SAFETY: realloc()'s room
        unsafe { copy_key(self.buf_mut().as_mut_ptr(), k) }
    }
}

/// Copies @k, header and value, to @dst.
///
/// # Safety
/// @dst has room for @k's u64s.
unsafe fn copy_key(dst: *mut u64, k: BkeySC<'_>) {
    let u64s = k.k.u64s as usize;
    // SAFETY: the caller's; a key's header and value are whole u64s
    unsafe {
        core::ptr::copy_nonoverlapping(core::ptr::from_ref(k.k).cast::<u64>(), dst, BKEY_U64S);
        core::ptr::copy_nonoverlapping(k.v.cast::<u64>(), dst.add(BKEY_U64S),
                                       u64s - BKEY_U64S);
    }
}

impl<'a> From<&'a BkeyBuf> for BkeySC<'a> {
    fn from(b: &'a BkeyBuf) -> Self {
        BkeySC::from(b.as_ref())
    }
}

/// The key, all of it, as long as its u64s say.
impl AsRef<BkeyI> for BkeyBuf {
    fn as_ref(&self) -> &BkeyI {
        // SAFETY: a key at the start of the buffer, as reassemble() sizes it
        unsafe { &*BkeyI::from_raw(self.buf().as_ptr().cast()) }
    }
}

impl AsMut<BkeyI> for BkeyBuf {
    fn as_mut(&mut self) -> &mut BkeyI {
        // SAFETY: as as_ref()'s
        unsafe { &mut *BkeyI::from_raw_mut(self.buf_mut().as_mut_ptr().cast()) }
    }
}

/// C's bkey_buf: @k points into @onstack while the key is small, so once
/// init()ed it can't move. Used for a data update's key, which C reads.
impl c::bkey_buf {
    pub fn key(&self) -> &BkeyI {
        // SAFETY: as C keeps it, for as long as it's borrowed
        unsafe { &*BkeyI::from_raw(self.k) }
    }

    fn onstack_ptr(&mut self) -> *mut c::bkey_i {
        self.onstack.as_mut_ptr().cast()
    }

    /// # Safety
    /// It isn't moved while it's in use: @k may point into it.
    pub unsafe fn init(&mut self) {
        self.k = self.onstack_ptr();
        // SAFETY: @onstack, which has room for a header
        unsafe { (*self.k).k = c::bkey::new() };
    }

    /// Copies @k in. A key too big for @onstack moves the buffer to the heap
    /// (__GFP_NOFAIL), where it stays.
    pub fn reassemble(&mut self, k: BkeySC<'_>) {
        const { assert!(size_of::<[u64; ONSTACK_U64S]>() == size_of::<c::bkey_buf>() - size_of::<*mut c::bkey_i>()) };

        if self.k == self.onstack_ptr() && k.k.u64s as usize > ONSTACK_U64S {
            let p = unsafe { c::rust_kzalloc_nofail(HEAP_U64S * size_of::<u64>()) }.cast::<u64>();
            assert!(!p.is_null(), "__GFP_NOFAIL allocation returned NULL");
            // SAFETY: a fresh buffer, room for any key
            unsafe { core::ptr::copy_nonoverlapping(self.onstack.as_ptr(), p, ONSTACK_U64S) };
            self.k = p.cast();
        }
        // SAFETY: init()'s @k, which is @onstack with room for @k, or the
        // heap buffer, with room for any key
        unsafe { copy_key(self.k.cast(), k) }
    }

    pub fn exit(&mut self) {
        if !self.k.is_null() && self.k != self.onstack_ptr() {
            // SAFETY: reassemble()'s
            unsafe { c::rust_kfree(self.k.cast()) };
        }
        self.k = core::ptr::null_mut();
    }

    /// exit(), for a key RCU readers may still be reading: the heap buffer is
    /// freed a grace period later, and @k is left pointing at it - so this must
    /// be the last thing done with the bkey_buf, and its memory freed no
    /// sooner.
    /// May sleep.
    pub fn exit_rcu(&mut self) {
        if !self.k.is_null() && self.k != self.onstack_ptr() {
            // SAFETY: reassemble()'s, kmalloc()'d
            unsafe { c::rust_kfree_rcu_mightsleep(self.k.cast()) };
        }
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
