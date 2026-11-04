// SPDX-License-Identifier: GPL-2.0

//! The btree write buffer - write_buffer.c's, as it converts. So far, what an
//! fsck pass uses to tell a key that's missing from one whose update is still
//! in the write buffer.

#![allow(non_camel_case_types)]

use crate::btree::bkey::BkeySC;
use crate::btree::iter::TransAttempt;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError};
use core::marker::PhantomData;
use core::ptr::NonNull;
use crate::fs::Fs;
use crate::util::Printbuf;
use crate::types::{DArray, c_default};
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_const, c_enum, c_extern, c_xmacro, TypeInfo};
use nestify::nest;
use crate::format::Bpos;

c_xmacro! {
    /*
     * Subset of BCH_BTREE_IDS() that are marked BTREE_IS_write_buffer, enumerated
     * densely so we can size per-btree write buffer state as a compact array. Must
     * be kept in sync with BCH_BTREE_IDS() — adding BTREE_IS_write_buffer to a new
     * btree requires a matching entry here.
     */
    BCH_WRITE_BUFFER_BTREES(x) {
        (accounting),
        (lru),
        (need_discard),
        (backpointers),
        (deleted_inodes),
        (reconcile_work),
        (reconcile_hipri),
        (reconcile_pending),
        (reconcile_work_phys),
        (reconcile_hipri_phys),
        (stripe_backpointers),
    }
}

c_enum! {
    #[closed]
    pub enum bch_wb_btree: u32 {
        BCH_WRITE_BUFFER_BTREES(name) => concat(BCH_WB_BTREE_, name),
        BCH_WB_BTREE_NR,
    }
}

c_const! {
    #[c_int]
    pub const BTREE_WRITE_BUFERED_VAL_U64s_MAX: u32 = 4;
}

/*
 * Each bch_fs_btree_write_buffer is per-btree, so individual key entries don't
 * need to carry a btree id — it's implicit in the containing buffer.
 */
#[cfg(target_endian = "little")]
nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]
    pub struct wb_key_ref {
        #[c_anon]
        #>[repr(C)]
        pub key: pub union wb_key_ref_key {
            #[c_anon]
            #>[repr(C, packed)]
            pub pos_idx: pub struct wb_key_ref_pos_idx {
                pub idx: u32,
                #[c("u8 pos[sizeof(struct bpos)]")]
                pub pos: [u8; size_of::<Bpos>()],
            },
            #[c_anon]
            #>[repr(C)]
            pub words: pub struct wb_key_ref_words {
                pub lo: u64,
                pub mi: u64,
                pub hi: u64,
            },
        },
    }
}

#[cfg(target_endian = "big")]
nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]
    pub struct wb_key_ref {
        #[c_anon]
        #>[repr(C)]
        pub key: pub union wb_key_ref_key {
            #[c_anon]
            #>[repr(C, packed)]
            pub pos_idx: pub struct wb_key_ref_pos_idx {
                #[c("u8 pos[sizeof(struct bpos)]")]
                pub pos: [u8; size_of::<Bpos>()],
                pub idx: u32,
            },
            #[c_anon]
            #>[repr(C)]
            pub words: pub struct wb_key_ref_words {
                pub hi: u64,
                pub mi: u64,
                pub lo: u64,
            },
        },
    }
}
c_default!(wb_key_ref);

#[repr(C)]
#[derive(Default, CStruct)]
pub struct btree_write_buffered_key {
    pub journal_seq: u64,

    /* BTREE_WRITE_BUFERED_VAL_U64s_MAX only applies to accounting keys */
    pub k: c::bkey_i,
    pub k_pad: [u64; c::BTREE_WRITE_BUFERED_VAL_U64s_MAX as usize],
}

#[repr(C)]
#[derive(CStruct)]
pub struct btree_write_buffer_keys {
    pub keys: c::darray_u64,
    #[c("struct journal_entry_pin pin")]
    pub pin: crate::util::ffi::Opaque<c::journal_entry_pin>,
    pub lock: c::mutex,
    /*
     * Back-references set at init so the journal-pin callback can recover
     * which btree (and which of inc/flushing) a firing pin belongs to.
     */
    pub wb_btree: c::bch_wb_btree,
    pub is_flushing: bool,
}
c_default!(btree_write_buffer_keys);

c_xmacro! {
    WB_FLUSH_CALLERS(x) {
        (thread),
        (journal_pin),
        (sync),
        (maybe),
        (tryflush),
    }
}

c_enum! {
    #[closed]
    pub enum wb_flush_caller: u32 {
        WB_FLUSH_CALLERS(n) => concat(WB_FLUSH_, n),
        WB_FLUSH_NR,
    }
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_write_buffer {
    /* Back-refs set at init so the flush thread can find c and its idx. */
    pub c: *mut Fs,
    pub idx: c::bch_wb_btree,

    pub sorted: DArray<c::wb_key_ref>,
    pub inc: c::btree_write_buffer_keys,
    pub flushing: c::btree_write_buffer_keys,

    pub flush_work: c::work_struct,
    /*
     * Set by the dispatcher (or auto-flush wakeup) before queue_work(),
     * read by the worker on each flush_locked call. Racy under concurrent
     * dispatchers — accepted; this only feeds the diagnostic
     * nr_flushes_caller[] histogram.
     */
    pub flush_work_caller: c::wb_flush_caller,

    pub nr_flushes: u64,
    pub nr_flushes_caller: [u64; c::WB_FLUSH_NR as usize],
    pub nr_keys_flushed: u64,
    pub nr_keys_fast: u64,
    pub nr_keys_slowpath: u64,
    pub nr_shards_total: u64,

    pub accounting: DArray<c::btree_write_buffered_key>,
}
c_default!(bch_fs_btree_write_buffer);

c_extern! {
    pub fn bch2_btree_write_buffer_flush_sync(trans: *mut c::btree_trans) -> c_int;
    pub fn bch2_btree_write_buffer_tryflush(trans: *mut c::btree_trans) -> c_int;
    pub fn bch2_btree_write_buffer_maybe_flush(trans: *mut c::btree_trans, referring_k: c::bkey_s_c,
                                               f: *mut c::wb_maybe_flush) -> c_int;
    pub fn bch2_btree_write_buffer_to_text(out: *mut Printbuf, c: *mut Fs);
    pub fn bch2_btree_write_buffer_flush_going_ro(c: *mut Fs) -> bool;
    pub fn bch2_btree_write_buffer_start(c: *mut Fs) -> c_int;
    pub fn bch2_btree_write_buffer_stop(c: *mut Fs);
}

impl Fs {
    pub fn btree_write_buffer_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_write_buffer_to_text(out.as_raw(), self.as_ptr()) }
    }
}

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

    pub fn inc(&mut self) {
        self.borrow().inc()
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

    /// Count a key checked: wb_maybe_flush_inc().
    pub fn inc(&mut self) {
        // SAFETY: borrowed
        unsafe { self.0.as_mut().nr_done += 1 };
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
    /// wb_maybe_flush_exit()
    fn drop(&mut self) {
        // SAFETY: ours, from alloc_pinned(), and nothing uses it after
        unsafe {
            self.0.as_mut().last_flushed.exit();
            free_pinned(self.0.as_ptr());
        }
    }
}
