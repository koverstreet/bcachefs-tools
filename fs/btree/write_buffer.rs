// SPDX-License-Identifier: GPL-2.0

/* DOC(btree-write-buffer)
 *
 * Batching layer for btrees that receive many small updates which are more
 * efficient to apply in bulk. Backpointers, LRU entries, and accounting
 * updates are written to the write buffer rather than directly to the btree;
 * the buffer is sorted and flushed periodically, coalescing updates to the
 * same key and amortizing the cost of btree traversal across many updates.
 */

#![allow(non_camel_case_types)]

mod flush;

use crate::accounting::{self, DiskAccountingPos};
use crate::btree::bkey::{BkeyI, BkeySC, BkeyV, BkeyVMut, BKEY_U64S};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::bch_fs_flags as Flag;
use crate::c::journal_pin_type::JOURNAL_PIN_TYPE_other;
use crate::c::wb_flush_caller as Caller;
use crate::data::keylist::PaddedKey;
use crate::data_structures::eytzinger::Eytzinger0;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::format::{BkeyType, Bpos, Btree, BtreeFlags};
use crate::fs::Fs;
use crate::locking::{MemallocFlags, RawLock, RawLockBase, RawTryLock};
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError, KVVec};
use crate::util::closure::closure_wake_up;
use crate::util::projections::Volatile;
use crate::util::workqueue::cancel_work_sync;
use crate::util::{Printbuf, ToText};
use crate::types::{c_default, rust_only};
use crate::{btree_trans, prt};
use core::ffi::c_int;
use core::marker::PhantomData;
use core::mem::{offset_of, size_of};
use core::ptr::{self, NonNull};
use core::sync::atomic::fence;
use cstruct_macros::{CStruct, c_const, c_enum, c_export, TypeInfo};
use nestify::nest;

#[cfg(CONFIG_BCACHEFS_TESTS)]
use core::sync::atomic::{AtomicI32, Ordering::Relaxed};

#[cfg(CONFIG_BCACHEFS_TESTS)]
crate::module_param! {
    /* One observation per module load, shared by all mounted filesystems. */
    static wb_pin_publication: bool = false;
    perm = 0o644, desc = "Force and observe flushing allocation failure once per module load";
}
#[cfg(CONFIG_BCACHEFS_TESTS)]
static TEST_WB_PIN_STATE: AtomicI32 = AtomicI32::new(0);

/*
 * Subset of BCH_BTREE_IDS() that are marked BTREE_IS_write_buffer, enumerated
 * densely so we can size per-btree write buffer state as a compact array. Must
 * be kept in sync with BCH_BTREE_IDS() — adding BTREE_IS_write_buffer to a new
 * btree requires a matching entry here.
 */
c_enum! {
    #[closed]
    #[prefix(BCH_WB_BTREE_)]
    #[nr(BCH_WB_BTREE_NR)]
    #[c_list(BCH_WRITE_BUFFER_BTREES)]
    pub enum bch_wb_btree: u32 {
        accounting,
        lru,
        need_discard,
        backpointers,
        deleted_inodes,
        reconcile_work,
        reconcile_hipri,
        reconcile_pending,
        reconcile_work_phys,
        reconcile_hipri_phys,
        stripe_backpointers,
    }
}

/*
 * Catch the "added BTREE_IS_write_buffer but forgot BCH_WRITE_BUFFER_BTREES"
 * footgun at compile time: count the write_buffer bits in BCH_BTREE_IDS() and
 * compare against BCH_WB_BTREE_NR.
 */
const _: () = assert!(crate::btrees_with(BtreeFlags::write_buffer).count_ones() == BCH_WB_BTREE_NR as u32,
                      "BCH_WRITE_BUFFER_BTREES() is out of sync with BCH_BTREE_IDS() write_buffer flags");

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
    #[c_anon("struct { struct bkey_i k; u64 k_pad[BTREE_WRITE_BUFERED_VAL_U64s_MAX]; }")]
    pub k: PaddedKey<{ BKEY_U64S + c::BTREE_WRITE_BUFERED_VAL_U64s_MAX as usize }>,
}

#[repr(C)]
#[derive(CStruct)]
pub struct btree_write_buffer_keys {
    pub keys: rust_only!(KVVec<u64>),
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

c_enum! {
    #[closed]
    #[prefix(WB_FLUSH_)]
    #[nr(WB_FLUSH_NR)]
    #[c_list(WB_FLUSH_CALLERS)]
    pub enum wb_flush_caller: u32 {
        thread,
        journal_pin,
        sync,
        maybe,
        tryflush,
    }
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_write_buffer {
    /* Back-refs set at init so the flush thread can find c and its idx. */
    pub c: *mut Fs,
    pub idx: c::bch_wb_btree,

    pub sorted: rust_only!(KVVec<c::wb_key_ref>),
    pub inc: c::btree_write_buffer_keys,
    pub flushing: c::btree_write_buffer_keys,

    pub flush_work: c::work_struct,
    /*
     * Set by the dispatcher (or auto-flush wakeup) before queue_work(),
     * read by the worker on each flush_locked call. Racy under concurrent
     * dispatchers — accepted; this only feeds the diagnostic
     * nr_flushes_caller[] histogram.
     */
    pub flush_work_caller: Volatile<u32>,

    pub nr_flushes: Volatile<u64>,
    pub nr_flushes_caller: [Volatile<u64>; c::WB_FLUSH_NR as usize],
    pub nr_keys_flushed: Volatile<u64>,
    pub nr_keys_fast: Volatile<u64>,
    pub nr_keys_slowpath: Volatile<u64>,
    pub nr_shards_total: Volatile<u64>,

    pub accounting: rust_only!(KVVec<c::btree_write_buffered_key>),
}
c_default!(bch_fs_btree_write_buffer);

/*
 * Growth heuristics only - every caller discards the result, and the buffer
 * works (more slowly) at its current size. Both call sites run under the
 * write buffer locks, which the journal write path and the flush path both
 * nest under j->buf_lock, so a caller that sits in direct reclaim here stalls
 * journal write completions. __GFP_NORETRY buys the failure immediately
 * instead of waiting for reclaim that a speculative resize doesn't merit.
 */
const WB_RESIZE_GFP: c::gfp_t = c::GFP_KERNEL | c::__GFP_NORETRY | c::__GFP_NOWARN;

/// Room for @new_size in all, rounded up to a power of two.
fn resize_gfp<T>(v: &mut KVVec<T>, new_size: usize, gfp: c::gfp_t) -> Result<(), AllocError> {
    if new_size <= v.capacity() {
        return Ok(());
    }
    v.reserve_raw_gfp(new_size.next_power_of_two() - v.len(), gfp)
}

impl c::btree_write_buffer_keys {
    /// # Safety
    /// Its lock is held.
    #[allow(clippy::mut_from_ref)]
    unsafe fn keys_mut(&self) -> &mut KVVec<u64> {
        unsafe { &mut *self.keys.as_ptr() }
    }

    /// # Safety
    /// Its lock is held.
    unsafe fn wb_keys_resize_locked(&self, new_size: usize) -> Result<(), AllocError> {
        resize_gfp(unsafe { self.keys_mut() }, new_size, WB_RESIZE_GFP)
    }

    fn wb_keys_resize(&self, new_size: usize) -> Result<(), BchError> {
        if self.keys.capacity() >= new_size {
            return Ok(());
        }

        if unsafe { self.lock.try_lock() }.is_none() {
            return Err(BchError::from(c::EINTR));
        }

        let ret = resize_gfp(unsafe { self.keys_mut() }, new_size, WB_RESIZE_GFP);
        unsafe { self.lock.unlock(()) };
        Ok(ret?)
    }
}

impl c::bch_fs_btree_write_buffer {
    fn fs(&self) -> &Fs {
        unsafe { &*self.c }
    }

    /// # Safety
    /// flushing.lock is held.
    #[allow(clippy::mut_from_ref)]
    unsafe fn sorted_mut(&self) -> &mut KVVec<c::wb_key_ref> {
        unsafe { &mut *self.sorted.as_ptr() }
    }

    /// # Safety
    /// inc.lock is held.
    #[allow(clippy::mut_from_ref)]
    unsafe fn accounting_mut(&self) -> &mut KVVec<c::btree_write_buffered_key> {
        unsafe { &mut *self.accounting.as_ptr() }
    }

    fn queue_flush(&self, caller: c::wb_flush_caller) {
        self.flush_work_caller.set(caller as u32);
        unsafe { self.fs().btree.write_buffer_wq.queue_work((&raw const self.flush_work).cast_mut()) };
    }

    fn should_flush(&self) -> bool {
        self.inc.keys.len() + self.flushing.keys.len() > self.inc.keys.capacity() / 4
    }

    fn should_resize(&self) -> bool {
        self.inc.keys.len() + self.flushing.keys.len() > self.inc.keys.capacity() / 2
    }

    fn accounting_sort(&self) {
        let accounting = unsafe { self.accounting_mut() };
        Eytzinger0::sort_by(accounting, |l, r| l.k.k.p.cmp(&r.k.k.p));
    }
}

/// A key in a write buffer's keys: its journal seq, then the key, as long as
/// the key's u64s - not a whole btree_write_buffered_key, whose padding may
/// run past the end.
struct WbKey<'a>(NonNull<c::btree_write_buffered_key>, PhantomData<&'a mut [u64]>);

impl<'a> WbKey<'a> {
    /// # Safety
    /// A key starts at @idx in @keys, which nothing else uses while it's held.
    unsafe fn at(keys: *mut u64, idx: usize) -> Self {
        WbKey(unsafe { NonNull::new_unchecked(keys.add(idx).cast()) }, PhantomData)
    }

    fn seq(&self) -> u64 {
        unsafe { (*self.0.as_ptr()).journal_seq }
    }

    fn set_seq(&mut self, seq: u64) {
        unsafe { (*self.0.as_ptr()).journal_seq = seq }
    }

    fn k(&self) -> &BkeyI {
        unsafe { &*BkeyI::from_raw((&raw const (*self.0.as_ptr()).k).cast()) }
    }

    fn k_mut(&mut self) -> &mut BkeyI {
        unsafe { &mut *BkeyI::from_raw_mut((&raw mut (*self.0.as_ptr()).k).cast()) }
    }

    fn u64s(&self) -> usize {
        wb_key_u64s(&self.k().k)
    }
}

const _: () = assert!(offset_of!(c::btree_write_buffered_key, journal_seq) == 0);

fn key_seq(keys: &[u64], idx: usize) -> u64 {
    keys[idx]
}

fn next_key(keys: &[u64], idx: usize) -> usize {
    idx + unsafe { WbKey::at(keys.as_ptr().cast_mut(), idx) }.u64s()
}

fn key_idxs(keys: &[u64]) -> impl Iterator<Item = usize> + '_ {
    core::iter::successors((!keys.is_empty()).then_some(0), move |&i| {
        Some(next_key(keys, i)).filter(|&n| n < keys.len())
    })
}

impl Fs {
    pub fn btree_write_buffer_must_wait(&self) -> bool {
        let (nr, size) = self.btree.write_buffer.iter().fold((0, 0), |(nr, size), wb| {
            (nr + wb.inc.keys.len() + wb.flushing.keys.len(), size + wb.inc.keys.capacity())
        });
        nr > size * 3 / 4
    }
}

#[c_export]
pub fn bch2_btree_write_buffer_must_wait(c: &Fs) -> bool {
    c.btree_write_buffer_must_wait()
}

/// @btree's slot among the write buffered btrees.
pub fn wb_btree_idx(btree: Btree) -> c::bch_wb_btree {
    macro_rules! idx {
        ($(($name:ident, $nr:expr)),* $(,)?) => {
            $(if btree == Btree::$name {
                return c::bch_wb_btree::$name;
            })*
        };
    }
    bch_wb_btree_arms!(idx);
    panic!("btree {btree} has no write buffer")
}

pub fn wb_btree_to_btree(idx: c::bch_wb_btree) -> Btree {
    macro_rules! btree {
        ($(($name:ident, $nr:expr)),* $(,)?) => {
            match idx {
                $(c::bch_wb_btree::$name => Btree::$name,)*
            }
        };
    }
    bch_wb_btree_arms!(btree)
}

fn insert_err(fs: &Fs, btree: Btree, k: &BkeyI) -> BchError {
    crate::fs_inconsistent!(fs, "attempting to do write buffer update on non wb btree={btree}\n{}",
                            BkeySC::from(k).to_text(fs));
    BchError::from(c::EROFS)
}

/// An update through the write buffer, for a btree that doesn't use it, is an
/// error.
pub fn insert_checks(fs: &Fs, btree: Btree, k: &BkeyI) -> Result<(), BchError> {
    if !btree.uses_write_buffer() {
        return Err(insert_err(fs, btree, k));
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

// ── Journal keys into the write buffer ───────────────────────────────────

/// # Safety
/// @k is an accounting key, and the write buffer's intake is in progress.
unsafe fn accounting_key_to_wb(fs: &Fs, btree: Btree, k: &BkeyI) -> Result<(), BchError> {
    let wb = &fs.btree.write_buffer[wb_btree_idx(btree) as usize];
    let p = k.k.p;

    let Some(idx) = Eytzinger0::find_by(&wb.accounting, |e| e.k.k.p.cmp(&p)) else {
        return unsafe { accounting_key_to_wb_slowpath(fs, btree, k) };
    };

    let dst = unsafe { &mut wb.accounting_mut()[idx] };
    accounting::accumulate(dst.k.key_mut(), BkeySC::from(k));
    Ok(())
}

/// # Safety
/// As accounting_key_to_wb()'s.
unsafe fn accounting_key_to_wb_slowpath(fs: &Fs, btree: Btree, k: &BkeyI) -> Result<(), BchError> {
    let wb = &fs.btree.write_buffer[wb_btree_idx(btree) as usize];

    fs.event_inc_trace(events::accounting_key_to_wb_slowpath, |out| {
        prt!(out, "have: {}\n", wb.accounting.len());
        prt!(out, "{}", BkeySC::from(k).to_text(fs));
    });

    let mut new = c::btree_write_buffered_key::default();
    new.k.set_key(k);

    unsafe { wb.accounting_mut() }.push(new)?;

    wb.accounting_sort();
    Ok(())
}

impl c::journal_keys_to_wb_btree {
    fn wb(&self) -> &c::btree_write_buffer_keys {
        unsafe { &*self.wb }
    }

    /// # Safety
    /// @k fits in this btree's buffer, acquired.
    unsafe fn key_reserved(&mut self, seq: u64, k: &BkeyI) {
        let u64s = wb_key_u64s(&k.k);
        let keys = unsafe { (*self.wb).keys_mut() };
        unsafe {
            let wb_k = keys.as_mut_ptr().add(keys.len()).cast::<c::btree_write_buffered_key>();
            (&raw mut (*wb_k).journal_seq).write(seq);
            ptr::copy_nonoverlapping(k.as_ffi().cast::<u64>(), (&raw mut (*wb_k).k).cast::<u64>(),
                                     usize::from(k.k.u64s));
            keys.inc_len(u64s);
        }
        self.room -= u64s;
    }
}

impl c::journal_keys_to_wb {
    /// # Safety
    /// As key_to_wb()'s.
    unsafe fn key_to_wb_idx(&mut self, fs: &Fs, idx: c::bch_wb_btree, k: &BkeyI) -> Result<(), BchError> {
        let seq = self.seq;
        let pb = &mut self.per_btree[idx as usize];

        if pb.room < wb_key_u64s(&k.k) {
            return unsafe { self.key_to_wb_slowpath(fs, idx, k) };
        }

        unsafe { pb.key_reserved(seq, k) };
        Ok(())
    }

    /// @k, from journal entry @seq, into @btree's write buffer.
    ///
    /// # Safety
    /// This is between keys_to_write_buffer_start() and _end().
    pub unsafe fn key_to_wb(&mut self, fs: &Fs, btree: Btree, k: &BkeyI) -> Result<(), BchError> {
        insert_checks(fs, btree, k)?;

        ebug_assert!(self.seq != 0, "journal key to the write buffer outside a journal write");

        if is_accounting_mem(&k.k) {
            unsafe { accounting_key_to_wb(fs, btree, k) }
        } else {
            unsafe { self.key_to_wb_idx(fs, wb_btree_idx(btree), k) }
        }
    }

    /// # Safety
    /// As key_to_wb()'s.
    unsafe fn key_to_wb_slowpath(&mut self, fs: &Fs, idx: c::bch_wb_btree, k: &BkeyI) -> Result<(), BchError> {
        let wb = &fs.btree.write_buffer[idx as usize];
        let u64s = wb_key_u64s(&k.k);
        let seq = self.seq;
        #[cfg(CONFIG_BCACHEFS_TESTS)]
        let mut force_alloc_failure;

        loop {
            let pb = &mut self.per_btree[idx as usize];
            let on_flushing = ptr::eq(pb.wb, &wb.flushing);
            /*
             * Growing flushing has somewhere to go if it fails - drop
             * flushing.lock and retry against inc, below - so don't wait on
             * reclaim for it. Taking the failure immediately is what releases
             * that lock, and both the journal write path and the flush path nest
             * the write buffer locks under j->buf_lock. Growing inc has no
             * fallback; that one has to be allowed to wait.
             */
            let gfp = if on_flushing { WB_RESIZE_GFP } else { c::GFP_KERNEL };

            let keys = unsafe { (*pb.wb).keys_mut() };

            #[cfg(CONFIG_BCACHEFS_TESTS)]
            {
                force_alloc_failure =
                    wb_pin_publication.get() &&
                    self.test_wb_pin_armed &&
                    idx == c::bch_wb_btree::backpointers &&
                    on_flushing &&
                    !keys.is_empty() &&
                    pb.wb().pin.seq() == 0 &&
                    TEST_WB_PIN_STATE.compare_exchange(1, 2, Relaxed, Relaxed).is_ok();
            }

            #[cfg(CONFIG_BCACHEFS_TESTS)]
            let mut ret = if force_alloc_failure {
                Err(AllocError)
            } else {
                resize_gfp(keys, keys.len() + u64s, gfp)
            };
            #[cfg(not(CONFIG_BCACHEFS_TESTS))]
            let mut ret = resize_gfp(keys, keys.len() + u64s, gfp);

            if ret.is_ok() && on_flushing {
                ret = resize_gfp(unsafe { wb.sorted_mut() }, wb.flushing.keys.capacity(), gfp);
            }

            if let Err(e) = ret {
                if on_flushing {
                    if !pb.wb().keys.is_empty() {
                        fs.journal.pin_add(seq, &pb.wb().pin, JOURNAL_PIN_TYPE_other, Some(journal_flush));
                    }
                    unsafe { pb.wb().lock.unlock(()) };
                    #[cfg(CONFIG_BCACHEFS_TESTS)]
                    if force_alloc_failure {
                        /*
                         * Reacquiring the lock proves what another lock holder can
                         * observe in the fallback window. Do not wait: a flush
                         * holder may need inc.lock or journal buf_lock from us.
                         * A holder may also drain flushing before we reacquire it;
                         * that is not an observation of the staged keys, so rearm.
                         */
                        if unsafe { wb.flushing.lock.try_lock() }.is_some() {
                            if !wb.flushing.keys.is_empty() {
                                kernel::pr_info!("WB_PIN_REPRO observer flushing={} fpin={}\n",
                                                 wb.flushing.keys.len(), wb.flushing.pin.seq());
                            } else {
                                kernel::pr_info!("WB_PIN_REPRO observer drained; retrying\n");
                                TEST_WB_PIN_STATE.store(0, Relaxed);
                            }
                            unsafe { wb.flushing.lock.unlock(()) };
                        } else {
                            kernel::pr_err!("WB_PIN_REPRO observer lock unavailable\n");
                            TEST_WB_PIN_STATE.store(0, Relaxed);
                        }
                        self.test_wb_pin_armed = false;
                    }
                    let pb = &mut self.per_btree[idx as usize];
                    pb.wb = (&raw const wb.inc).cast_mut();
                    fs.journal.pin_add(seq, &wb.inc.pin, JOURNAL_PIN_TYPE_other, Some(journal_flush));
                    continue;
                }

                return Err(e.into());
            }
            break;
        }

        let pb = &mut self.per_btree[idx as usize];
        let on_flushing = ptr::eq(pb.wb, &wb.flushing);
        pb.room = pb.wb().keys.capacity() - pb.wb().keys.len();
        if on_flushing {
            pb.room = pb.room.min(wb.sorted.capacity() - wb.flushing.keys.len());
        }
        assert!(pb.room >= u64s);
        assert!(seq != 0);

        unsafe { pb.key_reserved(seq, k) };
        #[cfg(CONFIG_BCACHEFS_TESTS)]
        if wb_pin_publication.get() &&
            idx == c::bch_wb_btree::backpointers &&
            on_flushing &&
            pb.wb().keys.len() == u64s &&
            pb.wb().pin.seq() == 0 &&
            TEST_WB_PIN_STATE.compare_exchange(0, 1, Relaxed, Relaxed).is_ok() {
            self.test_wb_pin_armed = true;
            let pb = &mut self.per_btree[idx as usize];
            pb.room = 0;
            kernel::pr_info!("WB_PIN_REPRO staged flushing={} fpin={}\n", pb.wb().keys.len(), pb.wb().pin.seq());
        }
        Ok(())
    }
}

/*
 * Acquire locks + intake state for one per-btree instance during one journal
 * intake pass. Called from _start() for every btree up front — eager, not
 * lazy, so the lock ordering is trivially "always ascending idx" and the
 * caller doesn't need to care about which btrees a journal buf actually
 * touches. Released in _end().
 *
 * Takes inc.lock (always), and flushing.lock (opportunistically, to let us
 * skip staging via inc if flushing has room).
 */
fn keys_to_write_buffer_lock(fs: &Fs, dst: &mut c::journal_keys_to_wb) {
    /*
     * Intake allocates under the write buffer locks (key array resizes,
     * accounting pushes), which sit on the reclaim path when swap is on
     * bcachefs: callers must have established a NOIO scope. Both current
     * callers hold j->buf_lock, a mutex_noio, across the whole bracket:
     */
    #[cfg(kernel)]
    ebug_assert!(crate::locking::memalloc_flags() & crate::locking::PF_MEMALLOC_NOIO != 0,
                 "write buffer intake outside a NOIO scope");

    *dst = c::journal_keys_to_wb::default();

    for (wb, pb) in fs.btree.write_buffer.iter().zip(dst.per_btree.iter_mut()) {
        unsafe {
            if wb.flushing.lock.try_lock().is_some() {
                wb.inc.lock.lock();
                flush::move_keys_from_inc_to_flushing(wb);

                if wb.inc.keys.is_empty() {
                    pb.wb = (&raw const wb.flushing).cast_mut();
                } else {
                    wb.flushing.lock.unlock(());
                    pb.wb = (&raw const wb.inc).cast_mut();
                }
            } else {
                wb.inc.lock.lock();
                pb.wb = (&raw const wb.inc).cast_mut();
            }
        }

        pb.room = pb.wb().keys.capacity() - pb.wb().keys.len();
        if ptr::eq(pb.wb, &wb.flushing) {
            pb.room = pb.room.min(wb.sorted.capacity() - wb.flushing.keys.len());
        }
        #[cfg(CONFIG_BCACHEFS_TESTS)]
        if wb_pin_publication.get() &&
            wb.idx == c::bch_wb_btree::backpointers &&
            ptr::eq(pb.wb, &wb.flushing) &&
            TEST_WB_PIN_STATE.load(Relaxed) == 0 {
            pb.room = 0;
        }
    }
}

fn keys_to_write_buffer_unlock(fs: &Fs, dst: &mut c::journal_keys_to_wb) {
    /*
     * For each btree: inject live (non-zero) accounting accumulators into
     * that btree's intake buffer, compact the darray if mostly-zero, then
     * drop the empty-keys pin and release the locks taken in _start().
     */
    for (wb, pb) in fs.btree.write_buffer.iter().zip(dst.per_btree.iter()) {
        let on_flushing = ptr::eq(pb.wb, &wb.flushing);

        if wb.should_flush() {
            wb.queue_flush(Caller::thread);
        }

        unsafe {
            if wb.should_resize() {
                let new_size = (1 << 24).min(wb.inc.keys.capacity() * 2);
                if on_flushing {
                    let _ = wb.flushing.wb_keys_resize_locked(new_size);
                }
                let _ = wb.inc.wb_keys_resize_locked(new_size);
            }

            if on_flushing {
                wb.flushing.lock.unlock(());
            }
            wb.inc.lock.unlock(());
        }
    }

    /*
     * Pins may have advanced (per-btree pin_drops above, plus any seqs
     * advanced by move_keys_from_inc_to_flushing inside bch_wb_acquire) —
     * wake any sync flushers blocked on a target seq.
     */
    closure_wake_up(&fs.btree.write_buffer_flush_wait);
}

fn accounting_reset(fs: &Fs) {
    let wb = &fs.btree.write_buffer[c::bch_wb_btree::accounting as usize];
    for i in unsafe { wb.accounting_mut() }.iter_mut() {
        (&mut i.k.key_mut()).val_bytes_mut().fill(0);
    }
}

/// # Safety
/// @dst is locked, by keys_to_write_buffer_lock().
unsafe fn journal_keys_to_write_buffer(fs: &Fs, dst: &mut c::journal_keys_to_wb, buf: &mut c::journal_buf)
    -> Result<(), BchError>
{
    for i in buf.data.jset_mut().entries_mut() {
        let Ok(i) = i else { break };

        if c::bch_jset_entry_type(i.type_.into()) != c::bch_jset_entry_type::write_buffer_keys {
            continue;
        }

        let btree = Btree(i.btree_id.into());
        for k in i.entries_mut() {
            let Ok(mut k) = k else { break };
            unsafe { dst.key_to_wb(fs, btree, &*BkeyI::from_raw(k.as_mut_ptr()))? };
        }

        i.type_ = c::bch_jset_entry_type::btree_keys.0 as u8;
    }

    Ok(())
}

fn fetch_wb_keys_from_journal(fs: &Fs, max_seq: u64) -> Result<(), BchError> {
    let j = &fs.journal;
    let mut dst = c::journal_keys_to_wb::default();
    let mut blocked = false;
    let mut ret = Ok(());

    let _buf_lock = j.buf_lock.lock();
    keys_to_write_buffer_lock(fs, &mut dst);

    while ret.is_ok() {
        let buf = j.next_write_buffer_flush_journal_buf(max_seq, &mut blocked);
        let Some(buf) = (unsafe { buf.as_mut() }) else { break };

        dst.seq = buf.data.head().seq.get();

        accounting_reset(fs);

        ret = unsafe { journal_keys_to_write_buffer(fs, &mut dst, buf) };
        ret = keys_to_write_buffer_end_locked(fs, &mut dst).and(ret);

        if !blocked && ret.is_ok() {
            let _g = j.lock.lock();
            buf.bits.set_need_flush_to_write_buffer(false);
        }

        if blocked {
            j.unblock();
            break;
        }
    }

    keys_to_write_buffer_unlock(fs, &mut dst);
    ret
}

/// The write buffer locked for a journal entry's keys, @seq's: key_to_wb()
/// takes them until keys_to_write_buffer_end().
pub fn keys_to_write_buffer_start(fs: &Fs, dst: &mut c::journal_keys_to_wb, seq: u64) {
    assert!(seq <= fs.journal_cur_seq());

    keys_to_write_buffer_lock(fs, dst);

    dst.seq = seq;

    /*
     * Zero accounting accumulators so per-buf deltas start fresh.
     * wb->accounting is invariantly protected by wb->inc.lock (all
     * intake paths that touch it — this reset, accumulate via
     * bch2_accounting_key_to_wb(), slowpath, and _end() inject /
     * compact / sort — run with inc.lock held).
     */
    accounting_reset(fs);
}

fn keys_to_write_buffer_end_locked(fs: &Fs, dst: &mut c::journal_keys_to_wb) -> Result<(), BchError> {
    let mut ret = Ok(());

    #[cfg(CONFIG_BCACHEFS_TESTS)]
    /* The forced failure must consume the arm in this intake. */
    if dst.test_wb_pin_armed {
        let _ = TEST_WB_PIN_STATE.compare_exchange(1, 0, Relaxed, Relaxed);
        dst.test_wb_pin_armed = false;
    }

    /*
     * For each btree: inject live (non-zero) accounting accumulators into
     * that btree's intake buffer, compact the darray if mostly-zero, then
     * drop the empty-keys pin and release the locks taken in _start().
     */
    for (i, wb) in fs.btree.write_buffer.iter().enumerate() {
        let accounting = unsafe { wb.accounting_mut() };
        let mut live_accounting_keys = 0;

        if ret.is_ok() {
            for a in accounting.iter_mut() {
                if !accounting::is_zero(BkeySC::from(a.k.key())) {
                    a.journal_seq = dst.seq;
                    live_accounting_keys += 1;
                    ret = unsafe { dst.key_to_wb_idx(fs, wb.idx, a.k.key()) };
                    if ret.is_err() {
                        break;
                    }
                }
            }
        }

        if live_accounting_keys * 2 < accounting.len() {
            accounting.retain(|a| !accounting::is_zero(BkeySC::from(a.k.key())));
            wb.accounting_sort();
        }

        let pb = &dst.per_btree[i];
        if !pb.wb().keys.is_empty() {
            fs.journal.pin_add(dst.seq, &pb.wb().pin, JOURNAL_PIN_TYPE_other, Some(journal_flush));
        }
    }

    ret
}

/// Done with keys_to_write_buffer_start()'s entry: the write buffer unlocked.
pub fn keys_to_write_buffer_end(fs: &Fs, dst: &mut c::journal_keys_to_wb) -> Result<(), BchError> {
    let ret = keys_to_write_buffer_end_locked(fs, dst);
    keys_to_write_buffer_unlock(fs, dst);
    ret
}

fn wb_pin_le(wb: &c::bch_fs_btree_write_buffer, max_seq: u64) -> bool {
    /*
     * move_keys_from_inc_to_flushing writes flushing.pin.seq (set), then
     * drops inc.pin.seq (clear).  Read inc first so that observing inc=0
     * after the drop means the following flushing read must observe the set
     * that preceded it.
     */
    let inc = wb.inc.pin.seq();
    fence(core::sync::atomic::Ordering::Acquire);
    let flushing = wb.flushing.pin.seq();
    (inc      != 0 && inc      <= max_seq) ||
    (flushing != 0 && flushing <= max_seq)
}

fn any_wb_pin_le(fs: &Fs, max_seq: u64, did_work: &mut bool, caller: c::wb_flush_caller) -> bool {
    for wb in &fs.btree.write_buffer {
        if wb_pin_le(wb, max_seq) {
            wb.queue_flush(caller);
            *did_work = true;
            return true;
        }
    }
    false
}

/*
 * Drive a fan-out flush across all per-btree write buffer instances:
 *
 *   1) fetch from the journal (snapshots cur_entry_offset_if_blocked,
 *      so max_seq + offset is well-defined),
 *   2) wake every per-btree flush worker, and
 *   3) wait for the workers to drain pins ≤ max_seq.
 *
 * The dispatcher used to call flush_locked() inline, serializing the per-btree
 * flushes; the toplevel sort is the expensive step and we want it parallelized.
 * @caller is recorded only by the dispatcher diagnostic counter (see below);
 * worker-driven flushes always count as WB_FLUSH_thread.
 */
fn flush_seq(trans: &BtreeTrans<'_>, max_seq: u64, did_work: &mut bool, caller: c::wb_flush_caller)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let mut ret = Ok(());

    trans.unlock();

    loop {
        let fetch_from_journal_err = fetch_wb_keys_from_journal(fs, max_seq);

        trans.wait_event(&fs.btree.write_buffer_flush_wait, || {
            !any_wb_pin_le(fs, max_seq, did_work, caller) || {
                ret = fs.journal.error();
                ret.is_err()
            }
        });

        if ret.is_err() || fetch_from_journal_err.is_ok() {
            return ret;
        }
    }
}

/*
 * Pin firing means keys are already staged in this btree's wb (inc.pin /
 * flushing.pin are added when keys land in the buffer — see bch_wb_acquire
 * and move_keys_from_inc_to_flushing). All we need to do is kick the per-btree
 * flush worker; it will drain the buffer and drop the pin asynchronously.
 *
 * Returning 0 here moves the pin to the per-type "flushed" list so reclaim
 * stops re-picking it; the worker's pin_drop removes it from the list when
 * the keys are actually written. No wait, no journal fetch needed.
 */
unsafe extern "C" fn journal_flush(j: *mut c::journal, pin: *mut c::journal_entry_pin, _seq: u64) -> c_int {
    let (fs, keys) = unsafe {
        ((*j).fs(), &*pin.byte_sub(offset_of!(c::btree_write_buffer_keys, pin)).cast::<c::btree_write_buffer_keys>())
    };

    fs.btree.write_buffer[keys.wb_btree as usize].queue_flush(Caller::journal_pin);
    0
}

impl BtreeTrans<'_> {
    /*
     * Drops the transaction's locks - it commits the buffered keys itself. Call
     * bch2_trans_begin() before touching the btree again.
     *
     * Every caller today satisfies that only by happening to follow this with a
     * for_each_btree_key* or commit_do, which begin on their own. Put a plain read
     * after it instead and you get "trans should be locked, unlocked by
     * btree_write_buffer_flush_seq" - at runtime, in a kernel build, and nowhere
     * that a tools build or a compile would show you.
     */
    pub fn write_buffer_flush_sync(&self) -> Result<(), BchError> {
        let fs = self.fs();
        let mut did_work = false;

        fs.event_inc_trace(events::write_buffer_flush_sync, |out| prt!(out, "{}", self.fn_name()));

        flush_seq(self, fs.journal_cur_seq(), &mut did_work, Caller::sync)
    }

    fn write_buffer_flush_nocheck_rw(&self) -> Result<(), BchError> {
        for wb in &self.fs().btree.write_buffer {
            if unsafe { wb.flushing.lock.try_lock() }.is_some() {
                self.unlock_long();
                let ret = unsafe { flush::flush_locked(self, wb, Caller::tryflush) };
                unsafe { wb.flushing.lock.unlock(()) };
                ret?;
            }
        }

        Ok(())
    }

    /// Flush what the btree write buffer holds, if anything - not waiting on
    /// a flush already running, and erofs_no_writes if the filesystem's
    /// going read-only. It may leave the transaction unlocked.
    pub fn write_buffer_tryflush(&self) -> Result<(), BchError> {
        let fs = self.fs();
        let write_ref = c::bch_write_ref::btree_write_buffer as u32;

        if !fs.btree.write_buffer.iter().any(|wb| !wb.inc.keys.is_empty() || !wb.flushing.keys.is_empty()) {
            return Ok(());
        }

        if !fs.writes.tryget_kind(write_ref) {
            return fs.throw(bch_errcode::BCH_ERR_erofs_no_writes);
        }

        let ret = self.write_buffer_flush_nocheck_rw();
        unsafe { fs.writes.put_kind(write_ref) };
        ret
    }
}

impl Fs {
    /*
     * The write buffer requires flushing when going RO: keys in the journal for the
     * write buffer don't have a journal pin yet
     */
    pub fn btree_write_buffer_flush_going_ro(&self) -> bool {
        if self.journal.error().is_err() {
            return false;
        }

        let trans = btree_trans!(self);
        let mut did_work = false;
        let _ = flush_seq(&trans, self.journal_cur_seq(), &mut did_work, Caller::sync);
        did_work
    }
}

/*
 * Drain this btree's write buffer until empty (or journal error).
 *
 * The auto-flush wakeup gate is per-btree bch_wb_btree_should_flush() at
 * journal_keys_to_write_buffer_end; this worker is also the fan-out target
 * for sync flushers, who don't go through that gate. So the worker drains
 * everything: once we're running, finish the job.
 *
 * After draining, wake the dispatcher's waitlist (sync flushers block here
 * waiting for pins ≤ their target seq to drain).
 */
unsafe extern "C" fn flush_work_fn(work: *mut c::work_struct) {
    let wb = unsafe {
        &*work.byte_sub(offset_of!(c::bch_fs_btree_write_buffer, flush_work)).cast::<c::bch_fs_btree_write_buffer>()
    };
    let fs = wb.fs();
    let caller = c::wb_flush_caller::from_bits(wb.flush_work_caller.get());

    {
        let _noio = MemallocFlags::noio();
        unsafe { wb.flushing.lock.lock() };
        let trans = btree_trans!(fs);
        loop {
            trans.unlock_long();

            /*
             * Accounting keys can't be flushed until
             * BCH_FS_accounting_replay_done - don't spin:
             */
            if unsafe { flush::flush_locked(&trans, wb, caller) }.is_err() ||
                (wb.inc.keys.is_empty() && wb.flushing.keys.is_empty()) ||
                !fs.flags.get(Flag::accounting_replay_done) {
                break;
            }
        }
        drop(trans);
        unsafe { wb.flushing.lock.unlock(()) };
    }

    closure_wake_up(&fs.btree.write_buffer_flush_wait);
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

    /*
     * In check and repair code, when checking references to write buffer btrees we
     * need to issue a flush before we have a definitive error: this issues a flush
     * if this is a key we haven't yet checked.
     */
    pub fn maybe_flush(&mut self, t: &TransAttempt<'_, '_>, referring_k: BkeySC<'_>) -> Result<(), BchError> {
        let (fs, trans) = (t.fs(), t.trans());
        let f = unsafe { self.0.as_mut() };

        if f.seen_error &&
            f.nr_flushes > 32 &&
            f.nr_flushes * 8 > f.nr_done {
            return Ok(());
        }

        /*
         * last_flushed caches "we flushed the write buffer while looking at this
         * key, so a re-read is current" - but a repair we then issue
         * (bch2_btree_bit_mod_buffered etc.) commits a new write buffer entry and
         * makes that stale. So the cache is also keyed on our own commit count:
         * ours, not the journal's, because another thread committing says nothing
         * about whether our read is current.
         */
        if !referring_k.and_val_eq(BkeySC::from(f.last_flushed.key())) ||
            f.flushed_commit_count != trans.commit_count() {
            fs.event_inc_trace(events::write_buffer_maybe_flush, |out| {
                prt!(out, "{}\n", trans.fn_name());
                prt!(out, "{}", referring_k.to_text(fs));
            });

            let tmp = BkeyBuf::from_key(referring_k);

            if referring_k.k.is_btree_ptr() &&
                fs.btree_interior_updates_pending() {
                trans.unlock_long();
                fs.btree_interior_updates_flush();
            }

            let mut did_work = false;
            flush_seq(trans, fs.journal_cur_seq(), &mut did_work, Caller::maybe)?;

            f.last_flushed.reassemble(BkeySC::from(&tmp));
            /* after the flush: it commits, and those commits are not a repair */
            f.flushed_commit_count = trans.commit_count();
            f.nr_flushes += 1;

            /* can we avoid the unconditional restart? */
            fs.event_inc_trace(events::trans_restart_write_buffer_flush, |out| prt!(out, "{}", trans.fn_name()));
            return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_write_buffer_flush));
        }

        f.seen_error = true;
        Ok(())
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

impl Fs {
    pub fn btree_write_buffer_resize(&self, new_size: usize) -> Result<(), BchError> {
        /* Reallocates the key arrays under the write buffer locks: */
        let _noio = MemallocFlags::noio();

        for wb in &self.btree.write_buffer {
            wb.flushing.wb_keys_resize(new_size)?;
            wb.inc.wb_keys_resize(new_size)?;
        }
        Ok(())
    }

    #[cold]
    pub fn btree_write_buffer_to_text(&self, out: &mut Printbuf) {
        if out.nr_tabstops() == 0 {
            out.tabstop_push(24);
        }

        for wb in &self.btree.write_buffer {
            if wb.nr_flushes.get() == 0 {
                continue;
            }

            prt!(out, "{}\n", wb.idx.name());
            let mut indent = out.indent(2);
            let out = &mut *indent;

            prt!(out, "inc keys:\t{}/{}\n",            wb.inc.keys.len(), wb.inc.keys.capacity());
            prt!(out, "inc seq pinned:\t{}\n",         wb.inc.pin.seq());

            prt!(out, "flushing keys:\t{}/{}\n",       wb.flushing.keys.len(), wb.flushing.keys.capacity());
            prt!(out, "flushing seq pinned:\t{}\n",    wb.flushing.pin.seq());

            prt!(out, "sorted:\t{}/{}\n",              wb.sorted.len(), wb.sorted.capacity());

            prt!(out, "nr flushes:\t{}\n",             wb.nr_flushes.get());
            for (j, n) in wb.nr_flushes_caller.iter().enumerate() {
                prt!(out, "  {}:\t{}\n", c::wb_flush_caller::from_bits(j as u32).name(), n.get());
            }
            prt!(out, "keys flushed:\t{}\n",           wb.nr_keys_flushed.get());
            prt!(out, "keys fast:\t{}\n",              wb.nr_keys_fast.get());
            prt!(out, "keys slowpath:\t{}\n",          wb.nr_keys_slowpath.get());
            prt!(out, "shards total:\t{}\n",           wb.nr_shards_total.get());
            if wb.nr_flushes.get() != 0 {
                prt!(out, "avg shards/flush:\t{}\n",   wb.nr_shards_total.get() / wb.nr_flushes.get());
            }
            if wb.nr_shards_total.get() != 0 {
                prt!(out, "avg shard size:\t{}\n",     wb.nr_keys_flushed.get() / wb.nr_shards_total.get());
            }

            let busy = unsafe { crate::util::workqueue::work_busy((&raw const wb.flush_work).cast_mut()) };
            prt!(out, "flush work:\t{}\n", if busy { "busy" } else { "idle" });

            prt!(out, "\n");
        }

        prt!(out, "Time stats (shared):\n");
        let mut indent = out.indent(2);
        self.times[c::bch_time_stats::btree_write_buffer_flush as usize].to_text(&mut indent);
    }

    pub fn btree_write_buffer_stop(&self) {
        for wb in &self.btree.write_buffer {
            unsafe { cancel_work_sync((&raw const wb.flush_work).cast_mut()) };
        }
    }

    pub fn btree_write_buffer_start(&self) -> Result<(), BchError> {
        /*
         * Work items are initialized once in init_early; nothing to start —
         * flushes are driven on demand by queue_work() from the wakeup path.
         */
        Ok(())
    }
}

#[c_export]
pub fn bch2_fs_btree_write_buffer_exit(c: &Fs) {
    for wb in &c.btree.write_buffer {
        assert!((wb.inc.keys.is_empty() && wb.flushing.keys.is_empty()) ||
                c.journal.error().is_err());

        unsafe {
            *wb.accounting_mut() = KVVec::new();
            *wb.sorted_mut() = KVVec::new();
            *wb.flushing.keys_mut() = KVVec::new();
            *wb.inc.keys_mut() = KVVec::new();
        }
    }
}

#[cfg(all(kernel, CONFIG_PROVE_LOCKING))]
unsafe extern "C" fn wb_keys_lock_cmp_fn(a: *const kernel::bindings::lockdep_map,
                                         b: *const kernel::bindings::lockdep_map) -> c_int {
    let keys = |m: *const kernel::bindings::lockdep_map| unsafe {
        &*m.byte_sub(offset_of!(kernel::bindings::mutex, dep_map) + offset_of!(c::btree_write_buffer_keys, lock))
            .cast::<c::btree_write_buffer_keys>()
    };

    keys(a).wb_btree.cmp(&keys(b).wb_btree) as c_int
}

/// # Safety
/// Before anything uses the write buffer: as C calls it, from bch2_fs_alloc().
#[c_export]
pub unsafe fn bch2_fs_btree_write_buffer_init_early(c: *mut Fs) {
    for i in 0..c::BCH_WB_BTREE_NR {
        unsafe {
            let wb = &raw mut (*c).btree.write_buffer[i as usize];
            let idx = c::bch_wb_btree::from_bits(i);

            (*wb).c                     = c;
            (*wb).idx                   = idx;
            (*wb).inc.wb_btree          = idx;
            (*wb).inc.is_flushing       = false;
            (*wb).flushing.wb_btree     = idx;
            (*wb).flushing.is_flushing  = true;
            let _ = <c::mutex as RawLockBase>::init(&raw mut (*wb).inc.lock, c"&wb->inc.lock",
                                                    crate::new_lock!(@key));
            let _ = <c::mutex as RawLockBase>::init(&raw mut (*wb).flushing.lock, c"&wb->flushing.lock",
                                                    crate::new_lock!(@key));
            #[cfg(all(kernel, CONFIG_PROVE_LOCKING))]
            {
                crate::locking::mutex_set_cmp_fn(&raw mut (*wb).inc.lock, wb_keys_lock_cmp_fn);
                crate::locking::mutex_set_cmp_fn(&raw mut (*wb).flushing.lock, wb_keys_lock_cmp_fn);
            }
            let _ = crate::util::alloc::init_at(crate::new_work!(flush_work_fn, "&wb->flush_work"),
                                                &raw mut (*wb).flush_work);
        }
    }
}

#[c_export]
pub fn bch2_fs_btree_write_buffer_init(c: &Fs) -> c_int {
    /*
     * Matches the journal's default buf size (JOURNAL_ENTRY_SIZE_MIN /
     * 64 slots); grows via bch2_btree_write_buffer_resize() as the journal
     * grows. With BCH_WB_BTREE_NR independent instances, keeping this small
     * matters — at 1<<16 the baseline was ~27 MB per fs.
     */
    let initial_size = 1 << 10;

    for wb in &c.btree.write_buffer {
        let ret = unsafe {
            resize_gfp(wb.inc.keys_mut(), initial_size, c::GFP_KERNEL)
                .and_then(|()| resize_gfp(wb.flushing.keys_mut(), initial_size, c::GFP_KERNEL))
                .and_then(|()| resize_gfp(wb.sorted_mut(), initial_size, c::GFP_KERNEL))
        };
        if let Err(e) = ret {
            return ret_to_c(Err(BchError::from(e)));
        }
    }
    0
}

#[c_export]
pub fn bch_wb_btree_idx(btree: Btree) -> c::bch_wb_btree {
    wb_btree_idx(btree)
}

/// # Safety
/// @k is a whole key.
#[c_export]
pub unsafe fn bch2_btree_write_buffer_insert_checks(c: &Fs, btree: Btree, k: *mut c::bkey_i) -> c_int {
    ret_to_c(insert_checks(c, btree, unsafe { &*BkeyI::from_raw(k) }))
}
