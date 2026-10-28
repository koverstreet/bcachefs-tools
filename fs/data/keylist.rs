// SPDX-License-Identifier: GPL-2.0

//! Keylists: whole keys back to back, from keys to top, in a buffer that's
//! inline in its owner - a write op's insert_keys, a btree split's parent
//! keys - until bch2_keylist_realloc() moves it to the heap: kmalloc()'d, as
//! keylist.h's bch2_keylist_free() kfree()s it. Keys go on at the top
//! (bch2_keylist_add(), keylist.h's) and come off the front, the rest moving
//! down.
//!
//! keylist.h keeps C's inlines; what keylist.c was is here, with what Rust
//! reads a keylist through.
//!
//! What's trusted is the keylist's invariant: keys..top is one buffer's,
//! written, top at or past keys. C keeps it - keylist.h, and the write path
//! and btree splits, which write keys at top, move top and index keys_p
//! themselves. as_u64s() takes it on trust, once, and everything else goes
//! through the slice - the keys walked as vstructs, each one's u64s checked
//! against what's left of it.
//!
//! The unsafe left is that boundary's, and goes when C stops touching the
//! fields - write.c's keylist handling and interior.c's in Rust:
//! - as_u64s(): the fields private, push()/add() Rust's, the invariant this
//!   module's own to keep.
//! - iter()'s assume_validated(): add() taking a validated key, so what's in
//!   a keylist is by type.
//! - krealloc() and the inline buffer's copy: free() Rust's too, the heap
//!   buffer an allocation that records its capacity - popping shrinks u64s,
//!   not the buffer, so today nothing knows its size.

use crate::btree::bkey::{BkeyI, BkeyV};
use crate::c;
use crate::locking::MemallocFlags;
use crate::util::alloc::krealloc;
use crate::util::vstructs::{assume_validated, plain_bytes, VstructRefEntries};
use core::ffi::c_int;
use core::mem::size_of;
use cstruct_macros::c_export;

impl c::keylist {
    /// # Safety
    /// @inline_keys is a buffer that outlives the list, keys written at its
    /// top() staying in it.
    pub unsafe fn init(&mut self, inline_keys: *mut u64) {
        self.keys_p = inline_keys;
        self.top_p = inline_keys;
    }

    pub fn top(&self) -> *mut c::bkey_i {
        self.top_p.cast()
    }

    /// # Safety
    /// A whole key is at top(), in the list's buffer.
    pub unsafe fn push(&mut self) {
        // SAFETY: the caller's
        let u64s = unsafe { (*self.top()).k.u64s };
        self.top_p = unsafe { self.top_p.add(u64s.into()) };
    }

    pub fn u64s(&self) -> usize {
        (self.top_p.addr() - self.keys_p.addr()) / size_of::<u64>()
    }

    pub fn is_empty(&self) -> bool {
        self.top_p == self.keys_p
    }

    fn as_u64s(&self) -> &[u64] {
        if self.is_empty() {
            return &[];
        }
        // SAFETY: the keylist's invariant
        unsafe { core::slice::from_raw_parts(self.keys_p, self.u64s()) }
    }

    fn as_u64s_mut(&mut self) -> &mut [u64] {
        if self.is_empty() {
            return &mut [];
        }
        // SAFETY: the keylist's invariant, and its holder lends it by &mut
        unsafe { core::slice::from_raw_parts_mut(self.keys_p, self.u64s()) }
    }

    pub fn iter(&self) -> impl Iterator<Item = &BkeyI> + '_ {
        VstructRefEntries::<c::bkey_i>::new(plain_bytes(self.as_u64s())).map(|k| {
            let k = k.unwrap_or_else(|e| panic!("a key overruns its keylist, of {} u64s: {e}", self.u64s()));
            // SAFETY: a keylist's keys are made in memory, by what adds them
            unsafe { assume_validated(k) }.key()
        })
    }

    pub fn front(&self) -> Option<&BkeyI> {
        self.iter().next()
    }

    /// The rest of the keys move down.
    pub fn pop_front(&mut self) {
        let n = usize::from(self.front().expect("popping the front of an empty keylist").k().u64s);
        let u64s = self.u64s();

        self.as_u64s_mut().copy_within(n.., 0);
        self.top_p = self.keys_p.wrapping_add(u64s - n);
    }
}

/// Room in @l for @new_u64s more: its buffer grown to the power of two that
/// holds them - kmalloc()'d, the inline buffer copied - unless what it has
/// already does. 0, or -ENOMEM.
///
/// # Safety
/// @l's buffer is @inline_u64s, @nr_inline_u64s long, or kmalloc()'s.
#[c_export]
pub unsafe fn bch2_keylist_realloc(l: &mut c::keylist, inline_u64s: *mut u64, nr_inline_u64s: usize,
                                   new_u64s: usize) -> c_int {
    let oldsize = l.u64s();
    let newsize = (oldsize + new_u64s).next_power_of_two();
    let old_buf = (l.keys_p != inline_u64s).then_some(l.keys_p);

    if newsize <= nr_inline_u64s || (old_buf.is_some() && oldsize.next_power_of_two() == newsize) {
        return 0;
    }

    let new = {
        let _noio = MemallocFlags::noio();
        // SAFETY: the keylist's heap buffer, or none: the caller's
        match unsafe { krealloc(old_buf.unwrap_or(core::ptr::null_mut()).cast(), newsize * size_of::<u64>()) } {
            Ok(p) => p.as_ptr().cast::<u64>(),
            Err(_) => return -(c::ENOMEM as c_int),
        }
    };

    if old_buf.is_none() {
        // SAFETY: the new buffer holds @newsize u64s, @oldsize or more
        unsafe { core::ptr::copy_nonoverlapping(l.as_u64s().as_ptr(), new, oldsize) };
    }

    l.keys_p = new;
    l.top_p = new.wrapping_add(oldsize);
    0
}

#[c_export]
pub fn bch2_keylist_pop_front(l: &mut c::keylist) {
    l.pop_front();
}

/// Its keys in increasing order - debug builds': keylist.h's no-op otherwise.
#[cfg(CONFIG_BCACHEFS_DEBUG)]
#[c_export]
pub fn bch2_verify_keylist_sorted(l: &c::keylist) {
    use crate::btree::bkey::bpos_ge;

    let mut keys = l.iter().peekable();
    while let Some(k) = keys.next() {
        if let Some(next) = keys.peek() {
            assert!(!bpos_ge(k.k().p, next.k().p), "keylist out of order: {} before {}", k.k(), next.k());
        }
    }
}
