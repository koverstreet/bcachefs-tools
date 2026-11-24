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
pub(in crate::btree) use flush::FlushKey;
use keys::{KeysLive, WbKeys};

use crate::btree::bkey::{BkeyI, BkeySC, BkeyV, BKEY_U64S};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::FsFlag as Flag;
use crate::c::JournalPinType::JOURNAL_PIN_TYPE_other;
use crate::c::WbFlushCaller as Caller;
use crate::data_structures::dst_room::DstRoom;
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::format::{Bpos, Btree, BtreeFlags};
use crate::fs::Fs;
use crate::journal::pins::{Pin, PinHolder};
use crate::locking::{ExclusiveGuard, Locked, MemallocFlags, Mutex};
use crate::util::alloc::{flags, Flags, AllocError, KVVec};
use crate::util::place::{Contained, FieldPath};
use crate::util::refcount::TryRefcount;
use crate::util::projections::Volatile;
use crate::os::workqueue::{OnQueue, Own, ReffedWork};
use crate::util::{Printbuf, ToText};
use crate::{btree_trans, new_lock, prt};
use core::mem::size_of;
use core::sync::atomic::fence;
use cstruct_macros::{placed, MaybeZeroable, Init, TypeInfo, bch_enum};
use pin_init::pin_data;

/*
 * Subset of BCH_BTREE_IDS() that are marked BTREE_IS_write_buffer, enumerated
 * densely so we can size per-btree write buffer state as a compact array. Must
 * be kept in sync with BCH_BTREE_IDS() — adding BTREE_IS_write_buffer to a new
 * btree requires a matching entry here.
 */
#[bch_enum(closed)]
#[nr]
#[repr(u32)]
pub enum WbBtree {
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

/*
 * Catch the "added BTREE_IS_write_buffer but forgot BCH_WRITE_BUFFER_BTREES"
 * footgun at compile time: count the write_buffer bits in BCH_BTREE_IDS() and
 * compare against WbBtree::NR.
 */
const _: () = assert!(crate::btrees_with(BtreeFlags::write_buffer).count_ones() == WbBtree::NR as u32,
                      "BCH_WRITE_BUFFER_BTREES() is out of sync with BCH_BTREE_IDS() write_buffer flags");

#[allow(non_upper_case_globals)]
pub const BTREE_WRITE_BUFERED_VAL_U64s_MAX: u32 = 4;

/*
 * Each bch_fs_btree_write_buffer is per-btree, so individual key entries don't
 * need to carry a btree id — it's implicit in the containing buffer.
 */
/// A key's ref in a flush's sort: its pos's bytes and its index, laid over
/// three words that compare as one 192 bit integer, hi:mi:lo - so by pos,
/// then index, the index the low 32 bits of lo.
#[derive(Clone, Copy, MaybeZeroable)]
#[repr(C)]
pub struct wb_key_ref {
    words: [u64; 3],
}

/* Where the index and the pos are in a ref's bytes, and which word is which */
#[cfg(target_endian = "little")]
const REF_IDX: core::ops::Range<usize> = 0..4;
#[cfg(target_endian = "little")]
const REF_POS: core::ops::Range<usize> = 4..4 + size_of::<Bpos>();
#[cfg(target_endian = "little")]
const REF_HI_MI_LO: [usize; 3] = [2, 1, 0];

#[cfg(target_endian = "big")]
const REF_POS: core::ops::Range<usize> = 0..size_of::<Bpos>();
#[cfg(target_endian = "big")]
const REF_IDX: core::ops::Range<usize> = size_of::<Bpos>()..size_of::<Bpos>() + 4;
#[cfg(target_endian = "big")]
const REF_HI_MI_LO: [usize; 3] = [0, 1, 2];

const _: () = assert!(REF_IDX.end - REF_IDX.start == size_of::<u32>() &&
                      REF_IDX.end - REF_IDX.start + REF_POS.end - REF_POS.start == size_of::<wb_key_ref>());

#[repr(C)]
#[derive(Default, MaybeZeroable)]
pub struct btree_write_buffered_key {
    pub journal_seq: u64,

    /* BTREE_WRITE_BUFERED_VAL_U64s_MAX only applies to accounting keys */
    pub k: DstRoom<BkeyI, [u64; BKEY_U64S + c::BTREE_WRITE_BUFERED_VAL_U64s_MAX as usize]>,
}

#[bch_enum(closed)]
#[nr]
#[repr(u32)]
pub enum WbFlushCaller {
    thread,
    journal_pin,
    sync,
    maybe,
    tryflush,
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
        pub nr_flushes_caller: [Volatile<u64>; c::WbFlushCaller::NR as usize],
        pub nr_keys_flushed:   Volatile<u64>,
        pub nr_keys_fast:      Volatile<u64>,
        pub nr_keys_slowpath:  Volatile<u64>,
        pub nr_shards_total:   Volatile<u64>,
    }
}

type IncGuard<'a> = ExclusiveGuard<'a, Mutex, Inc, KeysLive>;
type FlushingGuard<'a> = ExclusiveGuard<'a, Mutex, Flushing, FlushingLive>;

#[placed(c::bch_fs_btree.write_buffer[], index = idx: u32)]
#[pin_data]
#[repr(C)]
#[derive(TypeInfo, Init)]
pub struct bch_fs_btree_write_buffer {
    #[pin]
    #[init(new_lock!(Inc::default(), ::pin_init::zeroed(), "&wb->inc.lock"))]
    pub inc: Locked<Mutex, Inc, KeysLive>,
    pub inc_pin: Pin,

    #[pin]
    #[init(new_lock!(Flushing::default(), ::pin_init::zeroed(), "&wb->flushing.lock"))]
    pub flushing: Locked<Mutex, Flushing, FlushingLive>,
    pub flushing_pin: Pin,

    #[pin]
    pub(crate) flush_work: ReffedWork<flush_work_fn, OnQueue<crate::field!(super.write_buffer_wq)>>,
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
const WB_RESIZE_GFP: Flags = flags::NORETRY.union(flags::NOWARN);

/// Room for @new_size in all, rounded up to a power of two.
fn resize_gfp<T>(v: &mut KVVec<T>, new_size: usize, gfp: Flags) -> Result<(), AllocError> {
    if new_size <= v.capacity() {
        return Ok(());
    }
    v.reserve_gfp(new_size.next_power_of_two() - v.len(), gfp)
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

    fn wb_btree(&self) -> c::WbBtree {
        c::WbBtree::from_bits(self.idx.get())
    }

    fn pin(&self, b: Buf) -> &Pin {
        match b {
            Buf::Inc      => &self.inc_pin,
            Buf::Flushing => &self.flushing_pin,
        }
    }

    fn pin_add(&self, b: Buf, seq: u64) {
        self.fs().journal.pin_add(seq, self.pin(b), JOURNAL_PIN_TYPE_other)
    }

    fn pin_update(&self, b: Buf, seq: u64) {
        self.fs().journal.pin_update(seq, self.pin(b), JOURNAL_PIN_TYPE_other)
    }

    fn pin_drop(&self, b: Buf) {
        self.fs().journal.pin_drop(self.pin(b))
    }

    fn queue_flush(&self, caller: c::WbFlushCaller) {
        self.flush_work_caller.set(caller as u32);
        self.flush_work.queue(Own);
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
pub fn wb_btree_idx(btree: Btree) -> c::WbBtree {
    macro_rules! idx {
        ($(($name:ident, $nr:expr)),* $(,)?) => {
            $(if btree == Btree::$name {
                return c::WbBtree::$name;
            })*
        };
    }
    WbBtree_arms!(idx);
    panic!("btree {btree} has no write buffer")
}

pub fn wb_btree_to_btree(idx: c::WbBtree) -> Btree {
    macro_rules! btree {
        ($(($name:ident, $nr:expr)),* $(,)?) => {
            match idx {
                $(c::WbBtree::$name => Btree::$name,)*
            }
        };
    }
    WbBtree_arms!(btree)
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

fn any_wb_pin_le(fs: &Fs, max_seq: u64, did_work: &mut bool, caller: c::WbFlushCaller) -> bool {
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
fn flush_seq(trans: &BtreeTrans<'_>, max_seq: u64, did_work: &mut bool, caller: c::WbFlushCaller)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let mut ret = Ok(());

    trans.unlock();

    loop {
        let fetch_from_journal_err = intake::fetch_wb_keys_from_journal(fs, max_seq);

        trans.wait_until(&fs.btree.write_buffer_flush_wait, || {
            (!any_wb_pin_le(fs, max_seq, did_work, caller) || {
                ret = fs.journal.error();
                ret.is_err()
            }).then_some(())
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
 * Returning Ok here moves the pin to the per-type "flushed" list so reclaim
 * stops re-picking it; the worker's pin_drop removes it from the list when
 * the keys are actually written. No wait, no journal fetch needed.
 */
impl<P: FieldPath<Self, To = Pin>> PinHolder<P> for c::bch_fs_btree_write_buffer {
    fn journal_flush(&self, _fs: &Fs, _seq: u64) -> Result<(), BchError> {
        self.queue_flush(Caller::journal_pin);
        Ok(())
    }
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

        if fs.btree.write_buffer.iter().all(|wb| wb.is_empty()) {
            return Ok(());
        }

        let Some(_write) = fs.writes.tryget_live() else {
            return fs.throw(Errcode::erofs_no_writes);
        };

        self.write_buffer_flush_nocheck_rw()
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
    let caller = c::WbFlushCaller::from_bits(wb.flush_work_caller.get());

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

    fs.btree.write_buffer_flush_wait.wake();
}

/// C's struct wb_maybe_flush: before an fsck pass calls a key missing that
/// another key refers to, maybe_flush() flushes the write buffer - once per
/// referring key - and has it look again.
#[derive(Default)]
pub struct WbMaybeFlush {
    last_flushed:         BkeyBuf,
    flushed_commit_count: u32,
    nr_flushes:           u64,
    nr_done:              u64,
    seen_error:           bool,
}

impl WbMaybeFlush {
    /// wb_maybe_flush_init().
    pub fn new() -> Self {
        Self::default()
    }

    pub fn borrow(&mut self) -> WbFlush<'_> {
        WbFlush(self)
    }

    pub fn inc(&mut self) {
        self.borrow().inc()
    }

    pub fn maybe_flush(&mut self, t: &TransAttempt<'_, '_>, referring_k: BkeySC<'_>) -> Result<(), BchError> {
        self.borrow().maybe_flush(t, referring_k)
    }
}

/// A WbMaybeFlush borrowed.
pub struct WbFlush<'a>(&'a mut WbMaybeFlush);

impl WbFlush<'_> {
    pub fn reborrow(&mut self) -> WbFlush<'_> {
        WbFlush(self.0)
    }

    /// Count a key checked: wb_maybe_flush_inc().
    pub fn inc(&mut self) {
        self.0.nr_done += 1;
    }

    /*
     * In check and repair code, when checking references to write buffer btrees we
     * need to issue a flush before we have a definitive error: this issues a flush
     * if this is a key we haven't yet checked.
     */
    pub fn maybe_flush(&mut self, t: &TransAttempt<'_, '_>, referring_k: BkeySC<'_>) -> Result<(), BchError> {
        let (fs, trans) = (t.fs(), t.trans());
        let f = &mut *self.0;

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
        if !referring_k.and_val_eq(BkeySC::from(&f.last_flushed)) ||
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

            f.last_flushed.set_key(BkeySC::from(&tmp));
            /* after the flush: it commits, and those commits are not a repair */
            f.flushed_commit_count = trans.commit_count();
            f.nr_flushes += 1;

            /* can we avoid the unconditional restart? */
            fs.event_inc_trace(events::trans_restart_write_buffer_flush, |out| prt!(out, "{}", trans.fn_name()));
            return Err(t.restart(Errcode::transaction_restart_write_buffer_flush));
        }

        f.seen_error = true;
        Ok(())
    }

    /// The write buffer flushes it's done: a copy of a key taken before the
    /// count moved may be stale.
    pub fn nr_flushes(&self) -> u64 {
        self.0.nr_flushes
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
            let f = &*wb.flushing;
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
                prt!(out, "  {}:\t{}\n", c::WbFlushCaller::from_bits(j as u32).name(), n.get());
            }
            prt!(out, "keys flushed:\t{}\n",           f.nr_keys_flushed.get());
            prt!(out, "keys fast:\t{}\n",              f.nr_keys_fast.get());
            prt!(out, "keys slowpath:\t{}\n",          f.nr_keys_slowpath.get());
            prt!(out, "shards total:\t{}\n",           f.nr_shards_total.get());
            prt!(out, "avg shards/flush:\t{}\n",       f.nr_shards_total.get() / f.nr_flushes.get());
            if f.nr_shards_total.get() != 0 {
                prt!(out, "avg shard size:\t{}\n",     f.nr_keys_flushed.get() / f.nr_shards_total.get());
            }

            prt!(out, "flush work:\t{}\n", if wb.flush_work.busy() { "busy" } else { "idle" });

            prt!(out, "\n");
        }

        prt!(out, "Time stats (shared):\n");
        let mut indent = out.indent(2);
        self.times.get(c::TimeStat::btree_write_buffer_flush as usize).to_text(&mut indent);
    }

    pub fn btree_write_buffer_stop(&self) {
        for wb in &self.btree.write_buffer {
            wb.flush_work.cancel_sync();
        }
    }
}

impl Fs {
    pub(crate) fn btree_write_buffer_exit(&self) {
        for wb in &self.btree.write_buffer {
            assert!(wb.is_empty() || self.journal.error().is_err());
        }
    }
}

/* The write buffers are an array, in wb_btree order: so are their locks, of each class */
#[cfg(all(kernel, CONFIG_PROVE_LOCKING))]
unsafe extern "C" fn wb_keys_lock_cmp_fn(a: *const kernel::bindings::lockdep_map,
                                         b: *const kernel::bindings::lockdep_map) -> core::ffi::c_int {
    (a as usize).cmp(&(b as usize)) as core::ffi::c_int
}

impl Fs {
    pub(crate) fn btree_write_buffer_init(&self) -> Result<(), BchError> {
        /*
         * Matches the journal's default buf size (JOURNAL_ENTRY_SIZE_MIN /
         * 64 slots); grows via bch2_btree_write_buffer_resize() as the journal
         * grows. With WbBtree::NR independent instances, keeping this small
         * matters — at 1<<16 the baseline was ~27 MB per fs.
         */
        let initial_size = 1 << 10;

        for wb in &self.btree.write_buffer {
            #[cfg(all(kernel, CONFIG_PROVE_LOCKING))]
            {
                wb.inc.set_cmp_fn(wb_keys_lock_cmp_fn);
                wb.flushing.set_cmp_fn(wb_keys_lock_cmp_fn);
            }

            let mut flushing = wb.flushing.lock();
            let (f, f_live) = flushing.parts();
            let mut inc = wb.inc.lock();
            let (inc, inc_live) = inc.parts();

            let ret = inc.keys.resize(inc_live, initial_size, Flags::empty())
                .and_then(|()| f.keys.resize(&f_live.keys, initial_size, Flags::empty()))
                .and_then(|()| resize_gfp(&mut f.sorted, initial_size, Flags::empty()));
            f_live.sorted.set(f.sorted.len(), f.sorted.capacity());
            ret.map_err(BchError::from)?;
        }
        Ok(())
    }
}
