// SPDX-License-Identifier: GPL-2.0

use super::flush::move_keys_from_inc_to_flushing;
use super::keys::{wb_key_u64s, KeysLiveWriter, WbKeys};
use super::{insert_checks, resize_gfp, wb_btree_idx, Buf, FlushingGuard, IncGuard, WB_RESIZE_GFP};
use crate::accounting::{self, DiskAccountingPos};
use crate::btree::bkey::{BkeyI, BkeySC, BkeyV, BkeyVMut};
use crate::c;
use crate::c::WbFlushCaller as Caller;
use crate::data_structures::eytzinger::Eytzinger0;
use crate::debug::trace::events;
use crate::errcode::BchError;
use crate::format::{jset_entry, BkeyType, Btree};
use crate::fs::Fs;
use crate::prt;
use crate::util::alloc::AllocError;
use crate::util::closure::closure_wake_up;
use crate::util::vstructs::Unvalidated;

#[cfg(CONFIG_BCACHEFS_TESTS)]
use core::sync::atomic::{AtomicI32, Ordering::Relaxed};
#[cfg(all(CONFIG_BCACHEFS_TESTS, not(kernel)))]
use bcachefs_shim as kernel;

#[cfg(CONFIG_BCACHEFS_TESTS)]
crate::module_param! {
    /* One observation per module load, shared by all mounted filesystems. */
    static wb_pin_publication: bool = false;
    perm = 0o644, desc = "Force and observe flushing allocation failure once per module load";
}
#[cfg(CONFIG_BCACHEFS_TESTS)]
static TEST_WB_PIN_STATE: AtomicI32 = AtomicI32::new(0);

fn is_accounting_mem(k: &c::bkey) -> bool {
    k.key_type() == BkeyType::accounting && accounting::is_mem(&DiskAccountingPos::from_bpos(k.p))
}

/// A write buffered btree's part of a journal intake: its inc locked, and
/// its flushing too while its keys go straight there.
struct IntakeBtree<'a> {
    wb:       &'a c::bch_fs_btree_write_buffer,
    inc:      IncGuard<'a>,
    flushing: Option<FlushingGuard<'a>>,
    room:     usize,
}

impl<'a> IntakeBtree<'a> {
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
    fn lock(wb: &'a c::bch_fs_btree_write_buffer) -> Self {
        let (inc, flushing) = match wb.flushing.try_lock() {
            Some(mut flushing) => {
                let mut inc = wb.inc.lock();
                move_keys_from_inc_to_flushing(wb, &mut inc, &mut flushing);

                let flushing = inc.keys.is_empty().then_some(flushing);
                (inc, flushing)
            }
            None => (wb.inc.lock(), None),
        };

        let mut b = IntakeBtree { wb, inc, flushing, room: 0 };
        b.room = b.room_now();
        #[cfg(CONFIG_BCACHEFS_TESTS)]
        if wb_pin_publication.get() &&
            wb.wb_btree() == c::WbBtree::backpointers &&
            b.flushing.is_some() &&
            TEST_WB_PIN_STATE.load(Relaxed) == 0 {
            b.room = 0;
        }
        b
    }

    fn room_now(&self) -> usize {
        match &self.flushing {
            Some(f) => f.keys.room().min(f.sorted.capacity() - f.keys.len()),
            None    => self.inc.keys.room(),
        }
    }

    /// Where its keys go.
    fn buf(&self) -> Buf {
        if self.flushing.is_some() { Buf::Flushing } else { Buf::Inc }
    }

    fn keys_empty(&self) -> bool {
        match &self.flushing {
            Some(f) => f.keys.is_empty(),
            None    => self.inc.keys.is_empty(),
        }
    }

    /// buf()'s keys, and what publishes them.
    fn dst(&mut self) -> (&mut WbKeys, &KeysLiveWriter) {
        match &mut self.flushing {
            Some(f) => {
                let (f, live) = f.parts();
                (&mut f.keys, &live.keys)
            }
            None => {
                let (inc, live) = self.inc.parts();
                (&mut inc.keys, live)
            }
        }
    }

    /// @k, from journal entry @seq, into room checked.
    fn push(&mut self, seq: u64, k: &BkeyI) {
        let (keys, live) = self.dst();
        keys.push(live, seq, k);
        self.room -= wb_key_u64s(&k.k);
    }

    /// Room for @u64s more, and for flushing, sorted refs to match.
    fn grow(&mut self, u64s: usize, gfp: c::gfp_t) -> Result<(), AllocError> {
        let (keys, live) = self.dst();
        keys.resize(live, keys.len() + u64s, gfp)?;

        if let Some(f) = &mut self.flushing {
            let (f, live) = f.parts();
            resize_gfp(&mut f.sorted, f.keys.capacity(), gfp)?;
            live.sorted.set(f.sorted.len(), f.sorted.capacity());
        }
        Ok(())
    }

    fn should_flush(&self) -> bool {
        self.wb.inc.nr.get() + self.wb.flushing.keys.nr.get() > self.wb.inc.size.get() / 4
    }

    fn should_resize(&self) -> bool {
        self.wb.inc.nr.get() + self.wb.flushing.keys.nr.get() > self.wb.inc.size.get() / 2
    }

    fn accounting_sort(&mut self) {
        Eytzinger0::sort_by(&mut self.inc.accounting, |l, r| l.k.k.p.cmp(&r.k.k.p));
    }

    fn unlock(mut self) {
        let wb = self.wb;

        if self.should_flush() {
            wb.queue_flush(Caller::thread);
        }

        if self.should_resize() {
            let new_size = (1 << 24).min(self.inc.keys.capacity() * 2);
            if let Some(f) = &mut self.flushing {
                let (f, live) = f.parts();
                let _ = f.keys.resize(&live.keys, new_size, WB_RESIZE_GFP);
            }
            let (inc, live) = self.inc.parts();
            let _ = inc.keys.resize(live, new_size, WB_RESIZE_GFP);
        }
    }
}

/// The write buffer locked for the keys of journal entries: key_to_wb() takes
/// them, until it's dropped - start() for one entry, and end() after it, or
/// fetch_wb_keys_from_journal() for the entries the journal has.
pub struct Intake<'a> {
    fs:      &'a Fs,
    seq:     u64,
    #[cfg(CONFIG_BCACHEFS_TESTS)]
    test_wb_pin_armed: bool,
    /* None once dropped */
    btrees:  Option<[IntakeBtree<'a>; c::WbBtree::NR as usize]>,
}

impl<'a> Intake<'a> {
    fn lock(fs: &'a Fs) -> Self {
        /*
         * Intake allocates under the write buffer locks (key array resizes,
         * accounting pushes), which sit on the reclaim path when swap is on
         * bcachefs: callers must have established a NOIO scope. Both current
         * callers hold j->buf_lock, a mutex_noio, across the whole bracket:
         */
        #[cfg(kernel)]
        ebug_assert!(crate::os::sched::memalloc_flags() & crate::locking::PF_MEMALLOC_NOIO != 0,
                     "write buffer intake outside a NOIO scope");

        Intake {
            fs,
            seq: 0,
            #[cfg(CONFIG_BCACHEFS_TESTS)]
            test_wb_pin_armed: false,
            btrees: Some(core::array::from_fn(|i| IntakeBtree::lock(&fs.btree.write_buffer[i]))),
        }
    }

    fn btrees(&mut self) -> &mut [IntakeBtree<'a>] {
        self.btrees.as_mut().expect("an intake not yet dropped")
    }

    fn btree(&mut self, idx: c::WbBtree) -> &mut IntakeBtree<'a> {
        &mut self.btrees()[idx as usize]
    }

    /// The write buffer locked for a journal entry's keys, @seq's.
    pub fn start(fs: &'a Fs, seq: u64) -> Self {
        assert!(seq <= fs.journal_cur_seq());

        let mut dst = Self::lock(fs);
        dst.begin(seq);
        dst
    }

    /// For journal entry @seq's keys.
    fn begin(&mut self, seq: u64) {
        self.seq = seq;

        /*
         * Zero accounting accumulators so per-buf deltas start fresh.
         * wb->accounting is invariantly protected by wb->inc.lock (all
         * intake paths that touch it — this reset, accumulate via
         * bch2_accounting_key_to_wb(), slowpath, and _end() inject /
         * compact / sort — run with inc.lock held).
         */
        for i in self.btree(c::WbBtree::accounting).inc.accounting.iter_mut() {
            (&mut i.k.key_mut()).val_bytes_mut().fill(0);
        }
    }

    /// @k, from journal entry @seq, into @btree's write buffer.
    fn key_to_wb(&mut self, btree: Btree, k: &BkeyI) -> Result<(), BchError> {
        insert_checks(self.fs, btree, k)?;

        ebug_assert!(self.seq != 0, "journal key to the write buffer outside a journal write");

        if is_accounting_mem(&k.k) {
            self.accounting_key_to_wb(btree, k)
        } else {
            self.key_to_wb_idx(wb_btree_idx(btree), k)
        }
    }

    fn accounting_key_to_wb(&mut self, btree: Btree, k: &BkeyI) -> Result<(), BchError> {
        let fs = self.fs;
        let b = self.btree(wb_btree_idx(btree));
        let p = k.k.p;

        match Eytzinger0::find_by(&b.inc.accounting, |e| e.k.k.p.cmp(&p)) {
            Some(idx) => {
                accounting::accumulate(b.inc.accounting[idx].k.key_mut(), BkeySC::from(k));
                Ok(())
            }
            None => {
                fs.event_inc_trace(events::accounting_key_to_wb_slowpath, |out| {
                    prt!(out, "have: {}\n", b.inc.accounting.len());
                    prt!(out, "{}", BkeySC::from(k).to_text(fs));
                });

                let mut new = c::btree_write_buffered_key::default();
                new.k.set_key(k);

                b.inc.accounting.push(new)?;

                b.accounting_sort();
                Ok(())
            }
        }
    }

    fn key_to_wb_idx(&mut self, idx: c::WbBtree, k: &BkeyI) -> Result<(), BchError> {
        let seq = self.seq;
        let b = self.btree(idx);

        if b.room < wb_key_u64s(&k.k) {
            return self.key_to_wb_slowpath(idx, k);
        }

        b.push(seq, k);
        Ok(())
    }

    fn key_to_wb_slowpath(&mut self, idx: c::WbBtree, k: &BkeyI) -> Result<(), BchError> {
        let u64s = wb_key_u64s(&k.k);
        let seq = self.seq;

        loop {
            #[cfg(CONFIG_BCACHEFS_TESTS)]
            let armed = self.test_wb_pin_armed;
            let b = self.btree(idx);
            /*
             * Growing flushing has somewhere to go if it fails - drop
             * flushing.lock and retry against inc, below - so don't wait on
             * reclaim for it. Taking the failure immediately is what releases
             * that lock, and both the journal write path and the flush path nest
             * the write buffer locks under j->buf_lock. Growing inc has no
             * fallback; that one has to be allowed to wait.
             */
            let gfp = if b.flushing.is_some() { WB_RESIZE_GFP } else { c::GFP_KERNEL };

            #[cfg(CONFIG_BCACHEFS_TESTS)]
            let force_alloc_failure =
                wb_pin_publication.get() &&
                armed &&
                idx == c::WbBtree::backpointers &&
                b.flushing.is_some() &&
                !b.keys_empty() &&
                b.wb.pin(b.buf()).seq() == 0 &&
                TEST_WB_PIN_STATE.compare_exchange(1, 2, Relaxed, Relaxed).is_ok();
            #[cfg(not(CONFIG_BCACHEFS_TESTS))]
            let force_alloc_failure = false;

            let ret = if force_alloc_failure { Err(AllocError) } else { b.grow(u64s, gfp) };

            if let Err(e) = ret {
                let Some(flushing) = b.flushing.take() else {
                    return Err(e.into());
                };

                if !flushing.keys.is_empty() {
                    b.wb.pin_add(Buf::Flushing, seq);
                }
                drop(flushing);
                #[cfg(CONFIG_BCACHEFS_TESTS)]
                if force_alloc_failure {
                    /*
                     * Reacquiring the lock proves what another lock holder can
                     * observe in the fallback window. Do not wait: a flush
                     * holder may need inc.lock or journal buf_lock from us.
                     * A holder may also drain flushing before we reacquire it;
                     * that is not an observation of the staged keys, so rearm.
                     */
                    if let Some(f) = b.wb.flushing.try_lock() {
                        if !f.keys.is_empty() {
                            kernel::pr_info!("WB_PIN_REPRO observer flushing={} fpin={}\n",
                                             f.keys.len(), b.wb.pin(Buf::Flushing).seq());
                        } else {
                            kernel::pr_info!("WB_PIN_REPRO observer drained; retrying\n");
                            TEST_WB_PIN_STATE.store(0, Relaxed);
                        }
                    } else {
                        kernel::pr_err!("WB_PIN_REPRO observer lock unavailable\n");
                        TEST_WB_PIN_STATE.store(0, Relaxed);
                    }
                    self.test_wb_pin_armed = false;
                }
                self.btree(idx).wb.pin_add(Buf::Inc, seq);
                continue;
            }
            break;
        }

        let b = self.btree(idx);
        b.room = b.room_now();
        assert!(b.room >= u64s);
        assert!(seq != 0);

        b.push(seq, k);
        #[cfg(CONFIG_BCACHEFS_TESTS)]
        if wb_pin_publication.get() &&
            idx == c::WbBtree::backpointers &&
            b.flushing.as_ref().is_some_and(|f| f.keys.len() == u64s) &&
            b.wb.pin(Buf::Flushing).seq() == 0 &&
            TEST_WB_PIN_STATE.compare_exchange(0, 1, Relaxed, Relaxed).is_ok() {
            b.room = 0;
            kernel::pr_info!("WB_PIN_REPRO staged flushing={} fpin={}\n", u64s, b.wb.pin(Buf::Flushing).seq());
            self.test_wb_pin_armed = true;
        }
        Ok(())
    }

    /// Write buffer keys from @buf into the write buffer: their entries become
    /// btree keys.
    fn journal_buf(&mut self, buf: &mut c::journal_buf) -> Result<(), BchError> {
        for i in buf.data.jset_mut().entries_mut() {
            let Ok(i) = i else { break };

            if c::JsetEntryType(i.type_.into()) == c::JsetEntryType::write_buffer_keys {
                self.entry(i)?;
            }
        }

        Ok(())
    }

    /// A write_buffer_keys entry's keys into the write buffer, and the entry
    /// retyped to btree keys.
    pub fn entry(&mut self, i: &mut jset_entry<Unvalidated>) -> Result<(), BchError> {
        let btree = Btree(i.btree_id.into());
        for k in i.entries_mut() {
            let Ok(mut k) = k else { break };
            self.key_to_wb(btree, unsafe { &*BkeyI::from_raw(k.as_mut_ptr()) })?;
        }

        i.type_ = c::JsetEntryType::btree_keys.0 as u8;
        Ok(())
    }

    fn end_locked(&mut self) -> Result<(), BchError> {
        let seq = self.seq;
        let mut ret = Ok(());

        #[cfg(CONFIG_BCACHEFS_TESTS)]
        /* The forced failure must consume the arm in this intake. */
        if self.test_wb_pin_armed {
            let _ = TEST_WB_PIN_STATE.compare_exchange(1, 0, Relaxed, Relaxed);
            self.test_wb_pin_armed = false;
        }

        /*
         * For each btree: inject live (non-zero) accounting accumulators into
         * that btree's intake buffer, compact the darray if mostly-zero, then
         * drop the empty-keys pin and release the locks taken in _start().
         */
        for idx in (0..c::WbBtree::NR).map(c::WbBtree::from_bits) {
            let mut accounting = core::mem::take(&mut self.btree(idx).inc.accounting);
            let mut live_accounting_keys = 0;

            if ret.is_ok() {
                for a in accounting.iter_mut() {
                    if !accounting::is_zero(BkeySC::from(a.k.key())) {
                        a.journal_seq = seq;
                        live_accounting_keys += 1;
                        ret = self.key_to_wb_idx(idx, a.k.key());
                        if ret.is_err() {
                            break;
                        }
                    }
                }
            }

            let b = self.btree(idx);
            b.inc.accounting = accounting;

            if live_accounting_keys * 2 < b.inc.accounting.len() {
                b.inc.accounting.retain(|a| !accounting::is_zero(BkeySC::from(a.k.key())));
                b.accounting_sort();
            }

            if !b.keys_empty() {
                b.wb.pin_add(b.buf(), seq);
            }
        }

        ret
    }

    /// Done with start()'s entry: the write buffer unlocked.
    pub fn end(mut self) -> Result<(), BchError> {
        self.end_locked()
    }
}

impl Drop for Intake<'_> {
    fn drop(&mut self) {
        for b in self.btrees.take().into_iter().flatten() {
            b.unlock();
        }

        /*
         * Pins may have advanced (per-btree pin_drops above, plus any seqs
         * advanced by move_keys_from_inc_to_flushing inside bch_wb_acquire) —
         * wake any sync flushers blocked on a target seq.
         */
        closure_wake_up(&self.fs.btree.write_buffer_flush_wait);
    }
}

pub(super) fn fetch_wb_keys_from_journal(fs: &Fs, max_seq: u64) -> Result<(), BchError> {
    let j = &fs.journal;
    let mut blocked = false;
    let mut ret = Ok(());

    let _buf_lock = j.buf_lock.lock();
    let mut dst = Intake::lock(fs);

    while ret.is_ok() {
        let buf = j.next_write_buffer_flush_journal_buf(max_seq, &mut blocked);
        let Some(buf) = (unsafe { buf.as_mut() }) else { break };

        dst.begin(buf.data.head().seq.get());

        ret = dst.journal_buf(buf);
        ret = dst.end_locked().and(ret);

        if !blocked && ret.is_ok() {
            let _g = j.lock.lock();
            buf.bits.set_need_flush_to_write_buffer(false);
        }

        if blocked {
            j.unblock();
            break;
        }
    }

    drop(dst);
    ret
}
