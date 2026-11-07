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
mod intake;
mod keys;

pub use intake::Intake;
use keys::{KeysLive, WbKeys};

use crate::btree::bkey::{BkeyI, BkeySC, BkeyV, BKEY_U64S};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::bch_fs_flags as Flag;
use crate::c::journal_pin_type::JOURNAL_PIN_TYPE_other;
use crate::c::wb_flush_caller as Caller;
use crate::data::keylist::PaddedKey;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::format::{Bpos, Btree, BtreeFlags};
use crate::fs::Fs;
use crate::journal::pins::Pin;
use crate::locking::{ExclusiveGuard, Locked, MemallocFlags};
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError, KVVec};
use crate::util::closure::closure_wake_up;
use crate::util::place::{container_of, Contained};
use crate::util::projections::Volatile;
use crate::util::workqueue::{cancel_work_sync, run_raw, RawWork};
use crate::util::{Printbuf, ToText};
use crate::types::{c_default, rust_only, RustOnly};
use crate::{btree_trans, new_lock, prt};
use core::ffi::c_int;
use core::marker::PhantomData;
use core::mem::{offset_of, size_of};
use core::ptr::NonNull;
use core::sync::atomic::fence;
use cstruct_macros::{placed, CStruct, Init, TypeInfo, c_const, c_enum, c_export};
use nestify::nest;
use pin_init::pin_data;

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

/// What inc.lock protects: the keys the journal's intake stages, and the
/// accounting accumulators.
#[derive(Default)]
pub struct Inc {
    keys:       WbKeys,
    accounting: KVVec<c::btree_write_buffered_key>,
}

/// What flushing.lock protects: the keys a flush has, and their refs, sorted.
#[derive(Default)]
pub struct Flushing {
    keys:   WbKeys,
    sorted: KVVec<c::wb_key_ref>,
}

crate::published! {
    /// What flushing.lock's holder publishes: its keys' and its sorted refs'
    /// length and capacity, and the flush stats - read by to_text().
    #[derive(::pin_init::Zeroable)]
    pub struct FlushingLive {
        pub keys:              KeysLive,
        pub sorted:            KeysLive,
        pub nr_flushes:        Volatile<u64>,
        pub nr_flushes_caller: [Volatile<u64>; c::WB_FLUSH_NR as usize],
        pub nr_keys_flushed:   Volatile<u64>,
        pub nr_keys_fast:      Volatile<u64>,
        pub nr_keys_slowpath:  Volatile<u64>,
        pub nr_shards_total:   Volatile<u64>,
    }
}

type IncGuard<'a> = ExclusiveGuard<'a, c::mutex, Inc, KeysLive>;
type FlushingGuard<'a> = ExclusiveGuard<'a, c::mutex, Flushing, FlushingLive>;

/// inc's journal pin: placed, for its flush fn to have its write buffer.
#[placed(c::bch_fs_btree_write_buffer.inc_pin)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct wb_inc_pin {
    pub pin: c::journal_entry_pin,
}

/// flushing's journal pin, as inc's.
#[placed(c::bch_fs_btree_write_buffer.flushing_pin)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct wb_flushing_pin {
    pub pin: c::journal_entry_pin,
}

const _: () = assert!(offset_of!(wb_inc_pin, pin) == 0 && offset_of!(wb_flushing_pin, pin) == 0);

#[placed(c::bch_fs_btree.write_buffer[], index = idx: u32)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_btree_write_buffer {
    #[pin]
    #[init(RustOnly::new(new_lock!(Inc::default(), ::pin_init::zeroed(), "&wb->inc.lock")))]
    pub inc: rust_only!(Locked<c::mutex, Inc, KeysLive>),
    #[pin]
    pub inc_pin: c::wb_inc_pin,

    #[pin]
    #[init(RustOnly::new(new_lock!(Flushing::default(), ::pin_init::zeroed(), "&wb->flushing.lock")))]
    pub flushing: rust_only!(Locked<c::mutex, Flushing, FlushingLive>),
    #[pin]
    pub flushing_pin: c::wb_flushing_pin,

    #[pin]
    #[init(crate::new_work!(run_raw::<c::bch_fs_btree_write_buffer>, "&wb->flush_work"))]
    pub flush_work: c::work_struct,
    /*
     * Set by the dispatcher (or auto-flush wakeup) before queue_work(),
     * read by the worker on each flush_locked call. Racy under concurrent
     * dispatchers — accepted; this only feeds the diagnostic
     * nr_flushes_caller[] histogram.
     */
    pub flush_work_caller: Volatile<u32>,
}

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

/// A write buffer's two key buffers, as its journal pins are named.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Buf {
    Inc,
    Flushing,
}

impl c::bch_fs_btree_write_buffer {
    fn fs(&self) -> &Fs {
        self.container().container()
    }

    fn wb_btree(&self) -> c::bch_wb_btree {
        c::bch_wb_btree::from_bits(self.idx.get())
    }

    fn pin(&self, b: Buf) -> &Pin {
        match b {
            Buf::Inc      => &self.inc_pin.pin,
            Buf::Flushing => &self.flushing_pin.pin,
        }
    }

    fn pin_flush_fn(b: Buf) -> c::journal_pin_flush_fn {
        match b {
            Buf::Inc      => Some(journal_flush::<c::wb_inc_pin>),
            Buf::Flushing => Some(journal_flush::<c::wb_flushing_pin>),
        }
    }

    fn pin_add(&self, b: Buf, seq: u64) {
        self.fs().journal.pin_add(seq, self.pin(b), JOURNAL_PIN_TYPE_other, Self::pin_flush_fn(b))
    }

    fn pin_update(&self, b: Buf, seq: u64) {
        self.fs().journal.pin_update(seq, self.pin(b), JOURNAL_PIN_TYPE_other, Self::pin_flush_fn(b))
    }

    fn pin_drop(&self, b: Buf) {
        self.fs().journal.pin_drop(self.pin(b))
    }

    fn queue_flush(&self, caller: c::wb_flush_caller) {
        self.flush_work_caller.set(caller as u32);
        unsafe { self.fs().btree.write_buffer_wq.queue_work((&raw const self.flush_work).cast_mut()) };
    }

    fn is_empty(&self) -> bool {
        self.inc.nr.get() == 0 && self.flushing.keys.nr.get() == 0
    }

    fn wb_keys_resize(&self, b: Buf, new_size: usize) -> Result<(), BchError> {
        let ret = match b {
            Buf::Inc => {
                if self.inc.size.get() >= new_size {
                    return Ok(());
                }
                let Some(mut g) = self.inc.try_lock() else { return Err(BchError::from(c::EINTR)) };
                let (inc, live) = g.parts();
                inc.keys.resize(live, new_size, WB_RESIZE_GFP)
            }
            Buf::Flushing => {
                if self.flushing.keys.size.get() >= new_size {
                    return Ok(());
                }
                let Some(mut g) = self.flushing.try_lock() else { return Err(BchError::from(c::EINTR)) };
                let (f, live) = g.parts();
                f.keys.resize(&live.keys, new_size, WB_RESIZE_GFP)
            }
        };
        Ok(ret?)
    }
}

impl Fs {
    pub fn btree_write_buffer_must_wait(&self) -> bool {
        let (nr, size) = self.btree.write_buffer.iter().fold((0, 0), |(nr, size), wb| {
            (nr + wb.inc.nr.get() + wb.flushing.keys.nr.get(), size + wb.inc.size.get())
        });
        nr > size * 3 / 4
    }
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

fn wb_pin_le(wb: &c::bch_fs_btree_write_buffer, max_seq: u64) -> bool {
    /*
     * move_keys_from_inc_to_flushing writes flushing.pin.seq (set), then
     * drops inc.pin.seq (clear).  Read inc first so that observing inc=0
     * after the drop means the following flushing read must observe the set
     * that preceded it.
     */
    let inc = wb.pin(Buf::Inc).seq();
    fence(core::sync::atomic::Ordering::Acquire);
    let flushing = wb.pin(Buf::Flushing).seq();
    (inc      != 0 && inc      <= max_seq) ||
    (flushing != 0 && flushing <= max_seq)
}

fn any_wb_pin_le(fs: &Fs, max_seq: u64, did_work: &mut bool, caller: c::wb_flush_caller) -> bool {
    let wb = fs.btree.write_buffer.iter().find(|wb| wb_pin_le(wb, max_seq));
    if let Some(wb) = wb {
        wb.queue_flush(caller);
        *did_work = true;
    }
    wb.is_some()
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
        let fetch_from_journal_err = intake::fetch_wb_keys_from_journal(fs, max_seq);

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
unsafe extern "C" fn journal_flush<P: Contained<Parent = c::bch_fs_btree_write_buffer>>(
    _j: *mut c::journal, pin: *mut c::journal_entry_pin, _seq: u64) -> c_int
{
    let wb = unsafe { container_of(pin.cast::<P>()) };

    wb.queue_flush(Caller::journal_pin);
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
            if let Some(mut g) = wb.flushing.try_lock() {
                self.unlock_long();
                flush::flush_locked(self, wb, &mut g, Caller::tryflush)?;
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

        if fs.btree.write_buffer.iter().all(|wb| wb.is_empty()) {
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
fn flush_work_fn(wb: &c::bch_fs_btree_write_buffer) {
    let fs = wb.fs();
    let caller = c::wb_flush_caller::from_bits(wb.flush_work_caller.get());

    {
        let _noio = MemallocFlags::noio();
        let mut g = wb.flushing.lock();
        let trans = btree_trans!(fs);
        loop {
            trans.unlock_long();

            /*
             * Accounting keys can't be flushed until
             * BCH_FS_accounting_replay_done - don't spin:
             */
            if flush::flush_locked(&trans, wb, &mut g, caller).is_err() ||
                wb.is_empty() ||
                !fs.flags.get(Flag::accounting_replay_done) {
                break;
            }
        }
    }

    closure_wake_up(&fs.btree.write_buffer_flush_wait);
}

impl RawWork for c::bch_fs_btree_write_buffer {
    const WORK: usize = offset_of!(c::bch_fs_btree_write_buffer, flush_work);

    unsafe fn run(this: NonNull<Self>) {
        flush_work_fn(unsafe { this.as_ref() })
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
            wb.wb_keys_resize(Buf::Flushing, new_size)?;
            wb.wb_keys_resize(Buf::Inc, new_size)?;
        }
        Ok(())
    }

    #[cold]
    pub fn btree_write_buffer_to_text(&self, out: &mut Printbuf) {
        if out.nr_tabstops() == 0 {
            out.tabstop_push(24);
        }

        for wb in &self.btree.write_buffer {
            let f = &**wb.flushing;
            if f.nr_flushes.get() == 0 {
                continue;
            }

            prt!(out, "{}\n", wb.wb_btree().name());
            let mut indent = out.indent(2);
            let out = &mut *indent;

            prt!(out, "inc keys:\t{}/{}\n",            wb.inc.nr.get(), wb.inc.size.get());
            prt!(out, "inc seq pinned:\t{}\n",         wb.pin(Buf::Inc).seq());

            prt!(out, "flushing keys:\t{}/{}\n",       f.keys.nr.get(), f.keys.size.get());
            prt!(out, "flushing seq pinned:\t{}\n",    wb.pin(Buf::Flushing).seq());

            prt!(out, "sorted:\t{}/{}\n",              f.sorted.nr.get(), f.sorted.size.get());

            prt!(out, "nr flushes:\t{}\n",             f.nr_flushes.get());
            for (j, n) in f.nr_flushes_caller.iter().enumerate() {
                prt!(out, "  {}:\t{}\n", c::wb_flush_caller::from_bits(j as u32).name(), n.get());
            }
            prt!(out, "keys flushed:\t{}\n",           f.nr_keys_flushed.get());
            prt!(out, "keys fast:\t{}\n",              f.nr_keys_fast.get());
            prt!(out, "keys slowpath:\t{}\n",          f.nr_keys_slowpath.get());
            prt!(out, "shards total:\t{}\n",           f.nr_shards_total.get());
            prt!(out, "avg shards/flush:\t{}\n",       f.nr_shards_total.get() / f.nr_flushes.get());
            if f.nr_shards_total.get() != 0 {
                prt!(out, "avg shard size:\t{}\n",     f.nr_keys_flushed.get() / f.nr_shards_total.get());
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
}

#[c_export]
pub fn bch2_fs_btree_write_buffer_exit(c: &Fs) {
    for wb in &c.btree.write_buffer {
        assert!(wb.is_empty() || c.journal.error().is_err());
    }
}

/* The write buffers are an array, in wb_btree order: so are their locks, of each class */
#[cfg(all(kernel, CONFIG_PROVE_LOCKING))]
unsafe extern "C" fn wb_keys_lock_cmp_fn(a: *const kernel::bindings::lockdep_map,
                                         b: *const kernel::bindings::lockdep_map) -> c_int {
    (a as usize).cmp(&(b as usize)) as c_int
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
        #[cfg(all(kernel, CONFIG_PROVE_LOCKING))]
        {
            wb.inc.set_cmp_fn(wb_keys_lock_cmp_fn);
            wb.flushing.set_cmp_fn(wb_keys_lock_cmp_fn);
        }

        let mut inc = wb.inc.lock();
        let (inc, inc_live) = inc.parts();
        let mut flushing = wb.flushing.lock();
        let (f, f_live) = flushing.parts();

        let ret = inc.keys.resize(inc_live, initial_size, c::GFP_KERNEL)
            .and_then(|()| f.keys.resize(&f_live.keys, initial_size, c::GFP_KERNEL))
            .and_then(|()| resize_gfp(&mut f.sorted, initial_size, c::GFP_KERNEL));
        f_live.sorted.set(f.sorted.len(), f.sorted.capacity());
        if let Err(e) = ret {
            return ret_to_c(Err(BchError::from(e)));
        }
    }
    0
}
