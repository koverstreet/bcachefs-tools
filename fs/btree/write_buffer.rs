// SPDX-License-Identifier: GPL-2.0

//! The btree write buffer - write_buffer.c's, as it converts. So far, what an
//! fsck pass uses to tell a key that's missing from one whose update is still
//! in the write buffer.

#![allow(non_camel_case_types)]

use crate::accounting::{self, DiskAccountingPos};
use crate::btree::iter::BtreeTrans;
use crate::btree::bkey::{BkeyI, BkeySC};
use crate::btree::iter::TransAttempt;
use crate::c;
use crate::data_structures::eytzinger::Eytzinger0;
use crate::errcode::{ret_to_c, ret_to_result_void, BchError};
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError};
use core::marker::PhantomData;
use core::mem::{offset_of, size_of};
use core::ptr::{self, NonNull};
use crate::fs::Fs;
use crate::util::Printbuf;
use crate::types::{DArray, c_default};
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_const, c_enum, c_export, c_extern, c_xmacro, TypeInfo};
use nestify::nest;
use crate::format::{BkeyType, Bpos, Btree};

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
    pub pin: c::journal_entry_pin,
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
    pub fn bch2_btree_write_buffer_flush_sync(trans: *mut BtreeTrans<'_>) -> c_int;
    pub fn bch2_btree_write_buffer_tryflush(trans: *mut BtreeTrans<'_>) -> c_int;
    pub fn bch2_btree_write_buffer_maybe_flush(trans: *mut BtreeTrans<'_>, referring_k: c::bkey_s_c,
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

    pub fn btree_write_buffer_must_wait(&self) -> bool {
        let (nr, size) = self.btree.write_buffer.iter().fold((0, 0), |(nr, size), wb| {
            (nr + wb.inc.keys.nr + wb.flushing.keys.nr, size + wb.inc.keys.size)
        });
        nr > size * 3 / 4
    }
}

#[c_export]
pub fn bch2_btree_write_buffer_must_wait(c: &Fs) -> bool {
    c.btree_write_buffer_must_wait()
}

// ── Journal keys into the write buffer ───────────────────────────────────

c_extern! {
    pub fn bch2_btree_write_buffer_insert_err(c: *mut Fs, btree: Btree, k: *mut c::bkey_i) -> c_int;
    pub fn bch2_accounting_key_to_wb_slowpath(c: *mut Fs, btree: Btree, k: *mut c::bkey_i) -> c_int;
    pub fn bch2_journal_key_to_wb_slowpath(c: *mut Fs, dst: *mut c::journal_keys_to_wb, idx: c::bch_wb_btree,
                                           k: *mut c::bkey_i) -> c_int;
}

/// @btree's slot among the write buffered btrees.
pub fn wb_btree_idx(btree: Btree) -> c::bch_wb_btree {
    macro_rules! idx {
        ($(($name:ident)),* $(,)?) => { ::paste::paste! {
            $(if btree == Btree::$name {
                return c::bch_wb_btree::[<BCH_WB_BTREE_ $name>];
            })*
        } };
    }
    c::BCH_WRITE_BUFFER_BTREES!(idx);
    panic!("btree {btree} has no write buffer")
}

/// An update through the write buffer, for a btree that doesn't use it, is an
/// error.
pub fn insert_checks(fs: &Fs, btree: Btree, k: *mut c::bkey_i) -> Result<(), BchError> {
    if !btree.uses_write_buffer() {
        ret_to_result_void(unsafe { bch2_btree_write_buffer_insert_err(fs.as_ptr(), btree, k) })?;
    }
    Ok(())
}

/// Its size in the write buffer: the key, and its journal seq.
fn wb_key_u64s(k: &c::bkey) -> usize {
    usize::from(k.u64s) + offset_of!(c::btree_write_buffered_key, k) / size_of::<u64>()
}

fn is_accounting_mem(k: &c::bkey) -> bool {
    k.key_type() == BkeyType::accounting && accounting::is_mem(&DiskAccountingPos::from_bpos(k.p))
}

/// # Safety
/// @k is an accounting key.
unsafe fn accounting_key_to_wb(fs: &Fs, btree: Btree, k: *mut c::bkey_i) -> Result<(), BchError> {
    let wb = &fs.btree.write_buffer[wb_btree_idx(btree) as usize];
    let p = unsafe { (*k).k.p };

    let Some(idx) = Eytzinger0::find_by(wb.accounting.as_slice(), |e| e.k.k.p.cmp(&p)) else {
        return ret_to_result_void(unsafe { bch2_accounting_key_to_wb_slowpath(fs.as_ptr(), btree, k) });
    };

    let dst = (&raw const wb.accounting.as_slice()[idx].k).cast_mut();
    accounting::accumulate(unsafe { &mut *BkeyI::from_raw_mut(dst) }, BkeySC::from(unsafe { &*BkeyI::from_raw(k) }));
    Ok(())
}

impl c::journal_keys_to_wb_btree {
    /// # Safety
    /// @k fits in this btree's buffer, acquired.
    unsafe fn key_reserved(&mut self, seq: u64, k: *mut c::bkey_i) {
        let u64s = wb_key_u64s(unsafe { &(*k).k });
        let keys = unsafe { &mut (*self.wb).keys };
        unsafe {
            let wb_k = keys.data.add(keys.nr).cast::<c::btree_write_buffered_key>();
            (&raw mut (*wb_k).journal_seq).write(seq);
            ptr::copy_nonoverlapping(k.cast::<u64>(), (&raw mut (*wb_k).k).cast::<u64>(), usize::from((*k).k.u64s));
        }
        keys.nr += u64s;
        self.room -= u64s;
    }
}

impl c::journal_keys_to_wb {
    /// # Safety
    /// As key_to_wb()'s.
    unsafe fn key_to_wb_idx(&mut self, fs: &Fs, idx: c::bch_wb_btree, k: *mut c::bkey_i) -> Result<(), BchError> {
        let seq = self.seq;
        let this: *mut Self = self;
        let pb = &mut self.per_btree[idx as usize];

        if pb.room < wb_key_u64s(unsafe { &(*k).k }) {
            return ret_to_result_void(unsafe { bch2_journal_key_to_wb_slowpath(fs.as_ptr(), this, idx, k) });
        }

        unsafe { pb.key_reserved(seq, k) };
        Ok(())
    }

    /// @k, from journal entry @seq, into @btree's write buffer.
    ///
    /// # Safety
    /// @k is a whole key, and this is between bch2_journal_keys_to_write_buffer_start()
    /// and _end().
    pub unsafe fn key_to_wb(&mut self, fs: &Fs, btree: Btree, k: *mut c::bkey_i) -> Result<(), BchError> {
        insert_checks(fs, btree, k)?;

        ebug_assert!(self.seq != 0, "journal key to the write buffer outside a journal write");

        if is_accounting_mem(unsafe { &(*k).k }) {
            unsafe { accounting_key_to_wb(fs, btree, k) }
        } else {
            unsafe { self.key_to_wb_idx(fs, wb_btree_idx(btree), k) }
        }
    }
}

#[c_export]
pub fn bch_wb_btree_idx(btree: Btree) -> c::bch_wb_btree {
    wb_btree_idx(btree)
}

/// # Safety
/// @k is a whole key.
#[c_export]
pub unsafe fn bch2_btree_write_buffer_insert_checks(c: &Fs, btree: Btree, k: *mut c::bkey_i) -> c_int {
    ret_to_c(insert_checks(c, btree, k))
}

/// # Safety
/// As key_reserved()'s.
#[c_export]
pub unsafe fn bch2_journal_key_to_wb_reserved(_c: &Fs, pb: &mut c::journal_keys_to_wb_btree, seq: u64,
                                              k: *mut c::bkey_i) {
    unsafe { pb.key_reserved(seq, k) }
}

/// # Safety
/// As key_to_wb()'s.
#[c_export]
pub unsafe fn __bch2_journal_key_to_wb(c: &Fs, dst: &mut c::journal_keys_to_wb, idx: c::bch_wb_btree,
                                       k: *mut c::bkey_i) -> c_int {
    ret_to_c(unsafe { dst.key_to_wb_idx(c, idx, k) })
}

/// # Safety
/// As key_to_wb()'s.
#[c_export]
pub unsafe fn bch2_journal_key_to_wb(c: &Fs, dst: &mut c::journal_keys_to_wb, btree: Btree,
                                     k: *mut c::bkey_i) -> c_int {
    ret_to_c(unsafe { dst.key_to_wb(c, btree, k) })
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
