// SPDX-License-Identifier: GPL-2.0

//! The btree write buffer - write_buffer.c's, as it converts. So far, what an
//! fsck pass uses to tell a key that's missing from one whose update is still
//! in the write buffer.

use crate::btree::bkey::BkeySC;
use crate::btree::iter::TransAttempt;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError};
use core::marker::PhantomData;
use core::ptr::NonNull;

/// C's struct wb_maybe_flush: before an fsck pass calls a key missing that
/// another key refers to, maybe_flush() flushes the write buffer - once per
/// referring key - and has it look again. Its last_flushed is a bkey_buf whose
/// key starts in its own storage, so it's built in place, and never moves.
pub struct WbMaybeFlush(NonNull<c::wb_maybe_flush>);

impl WbMaybeFlush {
    /// wb_maybe_flush_init().
    pub fn new() -> Result<Self, AllocError> {
        // SAFETY: @f is zeroed memory, ours: its bkey_buf's key in its own
        // storage, holding a deleted key, as bch2_bkey_buf_init() leaves it
        let init = unsafe {
            ::pin_init::pin_init_from_closure(|f: *mut c::wb_maybe_flush| {
                let buf = &raw mut (*f).last_flushed;
                (*buf).k = (&raw mut (*buf).onstack).cast();
                (*(*buf).k).k = c::bkey::new();
                Ok::<_, AllocError>(())
            })
        };
        Ok(Self(NonNull::new(alloc_pinned(init)?).expect("alloc_pinned() succeeded")))
    }

    pub fn borrow(&mut self) -> WbFlush<'_> {
        WbFlush(self.0, PhantomData)
    }

    /// Count a key checked: wb_maybe_flush_inc().
    pub fn inc(&mut self) {
        // SAFETY: ours
        unsafe { self.0.as_mut().nr_done += 1 };
    }

    pub fn maybe_flush(&mut self, t: &TransAttempt<'_, '_>, referring_k: BkeySC<'_>) -> Result<(), BchError> {
        self.borrow().maybe_flush(t, referring_k)
    }
}

/// A WbMaybeFlush borrowed - the pass's own, or the struct wb_maybe_flush C
/// hands over. A handle, not a reference: a &mut to the struct would let it be
/// moved out from under its bkey_buf's key.
pub struct WbFlush<'a>(NonNull<c::wb_maybe_flush>, PhantomData<&'a mut c::wb_maybe_flush>);

impl WbFlush<'_> {
    /// None for NULL.
    ///
    /// # Safety
    /// @f is NULL, or live and used through nothing else for the handle's
    /// lifetime.
    pub unsafe fn from_raw(f: *mut c::wb_maybe_flush) -> Option<Self> {
        NonNull::new(f).map(|f| Self(f, PhantomData))
    }

    pub fn reborrow(&mut self) -> WbFlush<'_> {
        WbFlush(self.0, PhantomData)
    }

    /// Before calling what @referring_k refers to missing: flush the write
    /// buffer if it hasn't been for @referring_k, returning the restart that
    /// has the pass look again - bch2_btree_write_buffer_maybe_flush().
    pub fn maybe_flush(&mut self, t: &TransAttempt<'_, '_>, referring_k: BkeySC<'_>) -> Result<(), BchError> {
        // SAFETY: borrowed, for the call
        ret_to_result_void(unsafe {
            c::bch2_btree_write_buffer_maybe_flush(t.raw(), referring_k.into(), self.0.as_ptr())
        })
    }

    /// The write buffer flushes it's done: a copy of a key taken before the
    /// count moved may be stale.
    pub fn nr_flushes(&self) -> u64 {
        // SAFETY: borrowed
        unsafe { self.0.as_ref().nr_flushes }
    }

    /// For C that takes a struct wb_maybe_flush *.
    pub fn as_raw(&mut self) -> *mut c::wb_maybe_flush {
        self.0.as_ptr()
    }
}

impl Drop for WbMaybeFlush {
    /// wb_maybe_flush_exit(): a key grown past its own storage is on the heap.
    fn drop(&mut self) {
        // SAFETY: ours, from alloc_pinned(), and nothing uses it after
        unsafe {
            let buf = &raw mut (*self.0.as_ptr()).last_flushed;
            if (*buf).k.cast() != (&raw mut (*buf).onstack).cast::<u64>() {
                c::rust_kfree((*buf).k.cast());
            }
            free_pinned(self.0.as_ptr());
        }
    }
}
