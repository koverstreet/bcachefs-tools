// SPDX-License-Identifier: GPL-2.0

//! The reconcile thread: what keeps the filesystem's data in line with its
//! options. A key out of line - its replicas, checksum, compression, target
//! or erasure coding not what its options say - has a reconcile entry (see
//! format.rs) and a KEY_TYPE_set in a work btree; the thread walks the work
//! btrees, and for each key does what plan.rs says.
//!
//! # A pass
//!
//! A pass is PHASES in order, each walking a work btree from where it left
//! off:
//!
//! - Scan: requested scans, the cookies in reconcile_scan (scan.rs), which
//!   find work the work btrees don't have yet.
//! - Btree: btree nodes, by their backpointers in reconcile_scan at
//!   pos(work id, n) - btree nodes have no keys of their own to set bits for.
//! - Phys: work on rotational devices, from the phys work btrees - indexed by
//!   backpointer, so by position on disk - one thread per device: spinning
//!   rust wants device LBA order. The same work is in the logical btrees,
//!   where it's found done.
//! - Normal: the logical work btrees, by data position: data on SSDs.
//!
//! hipri work first - evacuations, rereplication - then normal, then
//! pending: work parked until the configuration changes, walked only after
//! a pending scan, which a device change requests. A pass that does nothing
//! - no work seen, no scan done - waits: on the write io clock, a fraction
//! of the smallest device's capacity written, or a kick.
//!
//! A kick (Kthread::kick()) is for new work: whoever makes it - a scan
//! requested, a device or option change - kicks the thread, and a pass that
//! sees one starts over from the first phase.
//!
//! # Work entries
//!
//! The buffered phases - phys, normal - read their entries ahead, a batch of
//! positions at a time (WorkBuf), keeping the walk out of the write buffer
//! flush's way. The scan btree's are read a key at a time: what's done to
//! each changes the next.
//!
//! # Stripes
//!
//! A stripe needing repair - a block on a device that's going, or lost its
//! durability - is a work entry too. Repair can need a block evacuated
//! first: the stripe's retried once the IO issued before it's done
//! (StripeRetry). A stripe that can't be repaired until something changes
//! is parked: kept in memory, in stripes_pending, and skipped - so reconcile
//! goes idle instead of retrying it back to back - until data moves, copygc
//! frees space, or devices change.

use crate::accounting;
use crate::alloc::buckets::DiskReservation;
use crate::alloc::backpointers::backpointer_get_key;
use crate::btree::bkey::{pos, BkeyI, BkeySC, BkeyV, POS_MIN};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{bkey_s_c_to_result, is_restart, lockrestart_do, polonius_key, BtreeIter, BtreeIterFlags,
                         BtreeTrans, CommitFlags, TransAttempt};
use crate::btree::write_buffer::WbMaybeFlush;
use crate::c;
use crate::data::ec::create::{stripe_repair, StripeRepair};
use crate::data::moving::{IoMark, MoveStats, MovingCtxt};
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::reconcile::needs_reconcile::update_reconcile_opts;
use crate::data::reconcile::plan::{park, parks_on, pending_mod, plan, Plan};
use crate::data::reconcile::scan::ScanCookie;
use crate::data::reconcile::trigger::rb_work_to_data_pos;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::{writepoint_ptr, Fs};
use crate::inode_opts::{get_io_opts, PerSnapshotIoOpts};
use crate::sb::counters::events;
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::alloc::KVVec;
use crate::util::cuckoo::Cuckoo;
use crate::util::kernel::{cond_resched, ktime_get_ns, WRITE};
use crate::util::kthread::{Kthread, KthreadSelf};
use crate::util::par::{self, Backend};
use crate::util::printbuf::ToText;
use crate::util::sleep::{sleep_until, Sleep};
use crate::util::Printbuf;
use crate::{bch_err, log_msg_ratelimited, warn_once, SPOS_MAX};
use c::bch_errcode as err;
use c::reconcile_work_id as w;
use core::cell::Cell;
use core::ffi::c_int;
use core::ops::ControlFlow;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_export;

impl Fs {
    /// Whether reconcile may run: not read-only, enabled, and if it's only to
    /// run on AC, not on battery. bch2_reconcile_enabled().
    pub fn reconcile_enabled(&self) -> bool {
        let o = self.opts();
        o.read_only == 0 && o.reconcile_enabled != 0 &&
            !(o.reconcile_on_ac_only != 0 && self.reconcile.on_battery.load(Relaxed))
    }
}

// ── Phases ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum PhaseKind {
    Scan,
    Btree,
    Phys,
    Normal,
}

impl PhaseKind {
    fn name(self) -> &'static str {
        match self {
            PhaseKind::Scan   => "scan",
            PhaseKind::Btree  => "btree",
            PhaseKind::Phys   => "phys",
            PhaseKind::Normal => "normal",
        }
    }
}

/// A walk of @btree from @start to @end, for work of @priority.
pub struct Phase {
    kind:     PhaseKind,
    priority: c::reconcile_work_id,
    btree:    c::btree_id,
    start:    c::bpos,
    end:      c::bpos,
}

impl Phase {
    /// Work waiting on a configuration change: walked only after a pending scan.
    fn is_pending(&self) -> bool {
        self.priority == w::RECONCILE_WORK_pending
    }
}

/// Work id @priority's btree node backpointers, in reconcile_scan.
const fn btree_phase(priority: c::reconcile_work_id) -> Phase {
    Phase { kind: PhaseKind::Btree, priority, btree: c::btree_id::reconcile_scan,
            start: pos(priority.0 as u64, 0), end: pos(priority.0 as u64, u64::MAX) }
}

const fn walk_phase(kind: PhaseKind, priority: c::reconcile_work_id, btree: c::btree_id) -> Phase {
    Phase { kind, priority, btree, start: POS_MIN, end: SPOS_MAX }
}

static PHASES: [Phase; 9] = [
    // scan cookies, at inode 0
    Phase { kind: PhaseKind::Scan, priority: w::RECONCILE_WORK_hipri, btree: c::btree_id::reconcile_scan,
            start: POS_MIN, end: pos(0, u64::MAX) },

    btree_phase(w::RECONCILE_WORK_hipri),
    // work in the phys btrees is in the logical ones too: phys first, then
    // what's left, data on SSDs
    walk_phase(PhaseKind::Phys,   w::RECONCILE_WORK_hipri,  c::btree_id::reconcile_hipri_phys),
    walk_phase(PhaseKind::Normal, w::RECONCILE_WORK_hipri,  c::btree_id::reconcile_hipri),

    btree_phase(w::RECONCILE_WORK_normal),
    walk_phase(PhaseKind::Phys,   w::RECONCILE_WORK_normal, c::btree_id::reconcile_work_phys),
    walk_phase(PhaseKind::Normal, w::RECONCILE_WORK_normal, c::btree_id::reconcile_work),

    btree_phase(w::RECONCILE_WORK_pending),
    walk_phase(PhaseKind::Normal, w::RECONCILE_WORK_pending, c::btree_id::reconcile_pending),
];

// ── Status ───────────────────────────────────────────────────────────────

/// What the thread is doing, as sysfs shows it.
#[derive(Clone, Copy)]
pub enum Status {
    /// In a pass: in @phase - None between phases - at @pos.
    Running { phase: Option<&'static Phase>, pos: c::bpos },
    /// Waiting for the write io clock to reach @iotime_end, from
    /// @iotime_start - since @since, ktime_get_ns() - or a kick.
    Waiting { iotime_start: u64, iotime_end: u64, since: u64 },
}

impl Status {
    pub const fn new() -> Self {
        Status::Waiting { iotime_start: 0, iotime_end: 0, since: 0 }
    }
}

fn set_status(fs: &Fs, s: Status) {
    fs.reconcile.status.write(|st| *st = s);
}

// ── Work entries ─────────────────────────────────────────────────────────

/// How many entries a buffered phase reads ahead.
const WORK_BUF_NR: usize = 1024;

/// The entries a buffered phase has read ahead and not yet done: positions in
/// its work btree, in order.
struct WorkBuf {
    pos:  KVVec<c::bpos>,
    next: usize,
}

impl WorkBuf {
    fn new() -> Result<Self, BchError> {
        Ok(WorkBuf { pos: KVVec::with_capacity(WORK_BUF_NR, GFP_KERNEL)?, next: 0 })
    }

    fn clear(&mut self) {
        self.pos.clear();
        self.next = 0;
    }

    /// The next entry, from @work_pos up to @end: a batch read once the last's
    /// done, and @work_pos moved past it - next_reconcile_entry().
    fn next(&mut self, trans: &BtreeTrans<'_>, work_pos: &mut c::bbpos, end: c::bpos)
        -> Result<Option<c::bpos>, BchError>
    {
        if self.next == self.pos.len() {
            self.clear();

            let fs = trans.fs();
            let flags = if work_pos.btree.has_snapshots() { BtreeIterFlags::ALL_SNAPSHOTS } else { BtreeIterFlags::empty() };
            let mut iter = BtreeIter::new(trans, work_pos.btree, work_pos.pos, BtreeIterFlags::PREFETCH | flags);

            iter.for_each_max(trans, end, |t, iter, k| {
                fs.reconcile.progress.update(t, iter)?;

                // leftover scan cookies from rebalance, from before the
                // reconcile upgrade
                if k.k.key_type() != c::bch_bkey_type::KEY_TYPE_set {
                    return Ok(ControlFlow::Continue(()));
                }

                self.pos.push(k.k.p, GFP_KERNEL).expect("room: WORK_BUF_NR reserved");
                work_pos.pos = iter.pos().successor();
                Ok(if self.pos.len() == WORK_BUF_NR { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
            })?;
        }

        let p = self.pos.get(self.next).copied();
        self.next += usize::from(p.is_some());
        Ok(p)
    }
}

/// The first key in @btree from @start to @end that @decode takes - a key at
/// a time, as what's done to one changes the next - and its position.
fn next_key<T>(trans: &BtreeTrans<'_>, btree: c::btree_id, start: c::bpos, end: c::bpos,
               mut decode: impl FnMut(BkeySC<'_>) -> Option<T>) -> Result<Option<(c::bpos, T)>, BchError>
{
    let mut iter = BtreeIter::new(trans, btree, start, BtreeIterFlags::PREFETCH);
    iter.for_each_max(trans, end, |_, _, k| Ok(match decode(k) {
        Some(v) => ControlFlow::Break(Some((k.k.p, v))),
        None    => ControlFlow::Continue(()),
    }))
}

// ── Stripes ──────────────────────────────────────────────────────────────

/// A stripe whose repair said StripeRepair::Retry: retried once the IO issued
/// before it - narrowing's evacuation - is done.
#[derive(Clone, Copy)]
struct StripeRetry {
    idx:  u64,
    mark: IoMark,
}

type StripeRetries = KVVec<StripeRetry>;

/// The parked stripes, by index: bch_fs_reconcile.stripes_pending.
pub type StripesPending = Cuckoo;

impl Fs {
    /// Park stripe @idx: see the top of the file. If it can't be - no memory
    /// - it's tried again next pass, as it would have been.
    fn stripe_park(&self, idx: u64) {
        let _ = self.reconcile.stripes_pending.insert(idx);
    }

    fn stripe_parked(&self, idx: u64) -> bool {
        self.reconcile.stripes_pending.contains(&idx)
    }

    /// Something changed that might let a parked stripe's repair succeed:
    /// data moved, copygc freed buckets, or the devices changed. The set is
    /// small, and a stripe that still can't be repaired parks again: retry
    /// them all.
    fn stripes_unpark(&self) {
        self.reconcile.stripes_pending.clear();
    }
}

/// Repair stripe @k, at @iter - or, for a repair to retry once its IO is
/// done, queue it on @retry, if this isn't the retry.
/// A stripe that can't be repaired is parked: one stripe must not end the
/// thread, and with it all background data movement until the next start.
fn reconcile_stripe(ctxt: &MovingCtxt<'_>, t: &TransAttempt<'_, '_>, iter: &mut BtreeIter<'_>, k: BkeySC<'_>,
                    retry: Option<&mut StripeRetries>) -> Result<(), BchError>
{
    let fs = t.fs();
    // a write buffer race
    let Some(s) = k.arm::<c::Stripe>().filter(|s| s.v().needs_reconcile() != 0) else { return Ok(()) };
    if fs.stripe_parked(k.k.p.offset) {
        return Ok(());
    }

    // Repair drops our locks to do IO: @k points into the btree - into the
    // key cache, for a retry - and may be freed or reused by the time it
    // returns. A copy, then:
    let copy = BkeyBuf::from_key(s.untyped());
    let s = BkeySC::from(&copy).arm::<c::Stripe>().expect("a copy of a stripe");
    let idx = s.k.p.offset;

    let ret = stripe_repair(ctxt, t, iter, s);

    fs.event_add_trace(events::reconcile_stripe, u64::from(s.v().sectors.get()) * u64::from(s.v().nr_blocks), |out| {
        write!(out, "{}\nret ", s.to_text(fs));
        match &ret {
            Ok(StripeRepair::Done)  => write!(out, "success"),
            Ok(StripeRepair::Retry) => write!(out, "retry"),
            Err(e)                  => write!(out, "{e}"),
        }
    });

    // A retry or an error ends the stripe's step - the attempts repair ran
    // were its own - and parking allocates, so not with our locks held
    match ret {
        Ok(StripeRepair::Done) => Ok(()),
        Ok(StripeRepair::Retry) => {
            t.restarts_handled();
            if let Some(retry) = retry {
                // not queued, for no memory: the next pass finds it again
                let _ = retry.push(StripeRetry { idx, mark: ctxt.io_mark() }, GFP_KERNEL);
            } else {
                // what the retry waited for - an evacuation - didn't finish it
                let mut msg = log_msg_ratelimited!(fs);
                write!(msg, "error retrying stripe: repair still not finished\n{}", s.to_text(fs));
                t.unlock();
                fs.stripe_park(idx);
            }
            Ok(())
        }
        Err(e) if !is_restart(&e) => {
            // out of space, say: left for a later pass, once something's changed
            let mut msg = log_msg_ratelimited!(fs);
            write!(msg, "error repairing stripe, leaving it for a later pass: {e}\n{}",
                   s.to_text(fs));
            t.restarts_handled();
            t.unlock();
            fs.stripe_park(idx);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ── Walking work entries ─────────────────────────────────────────────────

/// What a walk of work entries carries from step to step - the reconcile
/// thread's, for a pass, and each phys thread's: its moving context, and the
/// state its steps share.
struct Walker<'a, 'f> {
    ctxt:         &'a MovingCtxt<'f>,
    work:         WorkBuf,
    s:            PerSnapshotIoOpts,
    last_flushed: WbMaybeFlush,
    retry:        StripeRetries,
}

/// What do_reconcile() traces for a key: the move it started, if it did.
type Moved = Option<(c::bch_inode_opts, c::data_update_opts)>;

/// The iterator flags for reconciling a key in @btree: getting a stripe's
/// handle needs it intent locked, so the stripe trigger can't delete the
/// stripe meanwhile.
fn stripe_intent(btree: c::btree_id) -> BtreeIterFlags {
    if btree == c::btree_id::stripes { BtreeIterFlags::INTENT } else { BtreeIterFlags::empty() }
}

/// The trace a key's done with, @what for the event's message.
fn trace_moved(out: &mut Printbuf, fs: &Fs, what: impl FnOnce(&mut Printbuf), moved: &Moved) {
    write!(out, "\n");
    what(out);
    if let Some((opts, d)) = moved {
        write!(out, "\n");
        fs.data_update_opts_to_text(out, opts, d);
    }
}

impl<'a, 'f> Walker<'a, 'f> {
    fn new(ctxt: &'a MovingCtxt<'f>) -> Result<Self, BchError> {
        Ok(Walker {
            ctxt,
            work:         WorkBuf::new()?,
            s:            PerSnapshotIoOpts::new(ctxt.trans().fs()),
            last_flushed: WbMaybeFlush::new()?,
            retry:        StripeRetries::new(),
        })
    }

    /// The stripes waiting on a retry whose evacuations are done, retried -
    /// in order, up to one that isn't: do_retry_stripes().
    fn retry_stripes(&mut self) -> Result<(), BchError> {
        let ctxt = self.ctxt;

        for i in 0..self.retry.len() {
            let r = self.retry[i];
            if !ctxt.io_done(r.mark) {
                let left = self.retry.len() - i;
                self.retry.copy_within(i.., 0);
                self.retry.truncate(left);
                return Ok(());
            }

            lockrestart_do(ctxt.trans(), |t| {
                let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, r.idx),
                                              BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
                let k = iter.peek_slot(t)?.expect("a slot");
                // SAFETY: the key's in the btree - the key cache - not in
                // @iter, which reconcile_stripe() copies it from before
                // repair unlocks
                let k = unsafe { polonius_key(k) };
                reconcile_stripe(ctxt, t, &mut iter, k, None)
            })?;
        }
        self.retry.clear();
        Ok(())
    }

    /// Reconcile @k, a copy of the key at @iter, @level - from work entry
    /// @work, read from @read_dev if it's a phys phase's: its reconcile
    /// options brought up to date and committed, then its plan carried out.
    /// __do_reconcile_extent().
    fn key(&mut self, t: &TransAttempt<'_, '_>, work: c::bbpos, iter: &mut BtreeIter<'_>, level: u32,
           k: BkeySC<'_>, read_dev: Option<u32>) -> Result<Moved, BchError>
    {
        let ctxt = self.ctxt;
        if k.k.key_type() == c::bch_bkey_type::KEY_TYPE_stripe {
            return reconcile_stripe(ctxt, t, iter, k, Some(&mut self.retry)).map(|()| None);
        }
        if !k.k.extent_is_direct_data() {
            return Ok(None);
        }

        let fs = t.fs();
        ctxt.set_stats(MoveStats::from_c(&fs.reconcile.work_stats));

        let mut opts = get_io_opts(t, Some(&mut *self.s), k)?;
        update_reconcile_opts(t, Some(&mut *self.s), &opts, iter, level, k,
                              c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_other)?;

        // The reconcile opts are committed here, not carried into the data
        // update: move_extent() unlocks to start the IO, and past that the
        // attempt has no commit left to make.
        //
        // Not commit_lazy(), which signals success as
        // transaction_restart_commit: the phys phase walks the backpointers
        // btree, and a write buffer btree walk can't see the keys its own
        // commit just buffered - so the restart would find the same work at
        // the same position, forever.
        //
        // A commit invalidates keys peek() handed out, but @k is a copy.
        t.commit(Some(&DiskReservation::new(fs)), CommitFlags::NO_ENOSPC)?;

        let mut d = match plan(t, k, &opts)? {
            Plan::Nothing   => return Ok(None),
            Plan::Park(why) => return park(t, iter, level, k, why).map(|()| None),
            Plan::Move(d)   => d,
        };
        if let Some(dev) = read_dev {
            d.read_dev = dev;
            d.read_flags = c::bch_read_flags::BCH_READ_soft_require_read_device;
        }

        if work.btree == c::btree_id::reconcile_pending {
            match t.can_do_data_update(&opts, &d, k, None) {
                Ok(()) => {}
                // still can't: it stays parked
                Err(e) => return parks_on(t, &opts, &d, k, e).map(|()| None),
            }

            // The pending list is in logical inode:offset order, but data on
            // spinning rust wants device LBA order: off the pending list, for
            // the phys phase to find
            if k.arm::<Ptrs>().is_some_and(|kp| kp.ptrs(fs).any(|p| fs.dev_rotational(p.dev().into()))) {
                return pending_mod(t, iter, level, k, false).map(|()| None);
            }
        }

        match ctxt.move_extent(t, &mut opts, &mut d, iter, level, k) {
            Ok(()) => {}
            Err(e) if is_restart(&e) || e.matches(err::BCH_ERR_data_update_fail_need_copygc) => return Err(e),
            Err(e) => match parks_on(t, &opts, &d, k, e) {
                Ok(()) => pending_mod(t, iter, level, k, true)?,
                Err(e) if is_restart(&e) => return Err(e),
                Err(e) => {
                    warn_once!(!(e.matches(c::EROFS) ||
                                 e.matches(c::ENOMEM) ||
                                 e.matches(err::BCH_ERR_snapshot) ||
                                 e.matches(err::BCH_ERR_data_update_fail_no_snapshot) ||
                                 e.matches(err::BCH_ERR_data_update_fail_in_flight) ||
                                 e.matches(err::BCH_ERR_freelist_empty) ||
                                 e.matches(err::BCH_ERR_open_buckets_empty)),
                               fs, "unhandled error from move_extent: {e}");
                    // skip it, and go on
                }
            },
        }
        Ok(Some((opts, d)))
    }

    /// Work entry @work in a logical work btree: do_reconcile_extent().
    fn extent(&mut self, t: &TransAttempt<'_, '_>, work: c::bbpos) -> Result<(), BchError> {
        let fs = t.fs();
        let data_pos = rb_work_to_data_pos(work.pos);

        let mut iter = BtreeIter::new(t, data_pos.btree, data_pos.pos,
                                      BtreeIterFlags::ALL_SNAPSHOTS | stripe_intent(data_pos.btree));
        let Some(k) = iter.peek_slot(t)? else { return Ok(()) };
        let copy = BkeyBuf::from_key(k);
        let k = BkeySC::from(&copy);

        let moved = self.key(t, work, &mut iter, 0, k, None)?;

        fs.event_add_trace(events::reconcile_data, k.k.size.into(), |out| {
            trace_moved(out, fs, |out| write!(out, "{}", k.to_text(fs)), &moved);
        });
        Ok(())
    }

    /// Work entry @work in a phys work btree - a backpointer, on device
    /// work.pos.inode: do_reconcile_extent_phys().
    fn extent_phys(&mut self, t: &TransAttempt<'_, '_>, work: c::bbpos) -> Result<(), BchError> {
        let fs = t.fs();
        let mut bp_iter = BtreeIter::new(t, c::btree_id::backpointers, work.pos, BtreeIterFlags::empty());
        // none: a write buffer race
        let Some(bp) = bp_iter.peek_slot(t)?.and_then(BkeyI::<c::Backpointer>::from_key) else { return Ok(()) };

        if fs.data_update_in_flight(bp.v.target(), c::bch_data_update_types::BCH_DATA_UPDATE_reconcile) {
            return Ok(());
        }

        let mut iter = BtreeIter::uninit();
        let Some((_, k)) = backpointer_get_key(t, BkeySC::from(&bp), &mut iter, stripe_intent(bp.v.target().btree),
                                               self.last_flushed.borrow())? else { return Ok(()) };
        let copy = BkeyBuf::from_key(k);
        let k = BkeySC::from(&copy);

        let moved = self.key(t, work, &mut iter, bp.v.level.into(), k, Some(work.pos.inode as u32))?;

        fs.event_add_trace(events::reconcile_phys, k.k.size.into(), |out| {
            trace_moved(out, fs, |out| write!(out, "{}\n{}", BkeySC::<c::Backpointer>::from(&bp).to_text(fs),
                                              k.to_text(fs)), &moved);
        });
        Ok(())
    }

    /// Btree node backpointer @bp, at work entry @work: do_reconcile_btree().
    fn btree(&mut self, t: &TransAttempt<'_, '_>, work: c::bbpos, bp: &BkeyI<c::Backpointer>)
        -> Result<(), BchError>
    {
        let fs = t.fs();
        let mut iter = BtreeIter::uninit();
        // SAFETY: in the attempt; the key returned is the node's, copied
        // before anything unlocks
        let k = unsafe {
            bkey_s_c_to_result(c::reconcile_bp_get_key(t.raw(), iter.raw_mut(), BkeySC::from(bp).to_c()))
        }?;
        let Some(k) = k else { return Ok(()) };
        let copy = BkeyBuf::from_key(k);
        let k = BkeySC::from(&copy);

        let moved = self.key(t, work, &mut iter, bp.v.level.into(), k, None)?;

        fs.event_add_trace(events::reconcile_btree, fs.btree_sectors().into(), |out| {
            trace_moved(out, fs, |out| write!(out, "{}", k.to_text(fs)), &moved);
        });
        Ok(())
    }
}

// ── Phys: a thread per rotational device ─────────────────────────────────

/// Phase @phase's work on device @dev, in its own moving context, until it's
/// done - or anything fails: the next pass tries again. reconcile_phys_dev().
fn phys_dev(fs: &Fs, phase: &Phase, dev: u32) -> Result<(), BchError> {
    let stats = c::bch_move_stats::default();
    let ctxt = MovingCtxt::new(fs, MoveStats::from_c(&stats), writepoint_ptr(&fs.allocator.reconcile_write_point),
                               true)?;
    let mut walker = Walker::new(&ctxt)?;
    let mut work_pos = c::bbpos { btree: phase.btree, pos: pos(dev.into(), 0) };

    while ctxt.ratelimit().is_ok() && fs.reconcile_enabled() && !fs.flag(c::bch_fs_flags::BCH_FS_going_ro) {
        let trans = ctxt.trans();
        trans.begin_raw();

        let Ok(Some(p)) = walker.work.next(trans, &mut work_pos, pos(dev.into(), u64::MAX)) else { break };
        if p.inode != u64::from(dev) {
            break;
        }

        let work = c::bbpos { btree: work_pos.btree, pos: p };
        if lockrestart_do(trans, |t| walker.extent_phys(t, work)).is_err() {
            break;
        }
    }
    Ok(())
}

/// Phase @phase, a phys phase: a freezable kthread per rotational device -
/// not a work item: the freezer can only stop a work item between items,
/// and this runs for a whole pass, waiting on moves the freezer has already
/// stopped. A kthread is frozen in place in its move waits, like the other
/// movers. A device's that can't start is reported, and the rest go on:
/// do_reconcile_phys().
fn reconcile_phys(fs: &Fs, phase: &'static Phase) -> Result<(), BchError> {
    let mut devs = KVVec::new();
    for ca in fs.members().filter(|ca| ca.mi.rotational != 0 && ca.is_online()) {
        devs.push(u32::from(ca.dev_idx), GFP_KERNEL)?;
    }
    let name = fs.name().to_str().unwrap_or("?");

    par::scope_freezable(Backend::Kthreads, |sc| {
        for &dev in devs.iter() {
            sc.spawn(format_args!("bch-reconcile/{name}:{dev}"), move || {
                if let Err(e) = phys_dev(fs, phase, dev) {
                    bch_err!(fs, "reconcile phys thread for device {dev}: {e}");
                }
                Ok(())
            })?;
        }
        Ok(())
    })
}

// ── A pass ───────────────────────────────────────────────────────────────

/// The thread's, across passes.
struct Reconciler<'f, 'm> {
    fs:   &'f Fs,
    me:   &'m KthreadSelf<'m>,
    ctxt: MovingCtxt<'f>,
    /// copygc's run count when parked stripes were last let go, for freed space
    copygc_runs_unparked: Cell<u32>,
}

/// A pass's own: do_reconcile()'s reconcile_pass.
struct Pass<'a, 'f> {
    w:               Walker<'a, 'f>,
    /// The kicks seen when the phases last started: a kick since starts
    /// them over
    kick:            u32,
    /// A pending scan's, found in the scan phase: the pending phases run
    /// only with one, and complete it after
    pending:         Option<ScanCookie>,
    sectors_scanned: u64,
    copygc_runs:     u32,
}

/// Why the phases stopped short, most final first.
#[derive(Clone, Copy)]
enum Interrupt {
    /// Ratelimited - which a stop is too - or going read-only: the pass ends.
    Stopped,
    /// Reconcile disabled, or on battery: the phases wait until it isn't.
    Disabled,
    /// Kicked, for new work: the phases start over.
    Kicked,
}

impl<'f> Reconciler<'f, '_> {
    /// What stops a phase, if anything, given the kicks seen when the phases
    /// started.
    fn interrupted(&self, kick: u32) -> Option<Interrupt> {
        if self.ctxt.ratelimit().is_err() || self.fs.flag(c::bch_fs_flags::BCH_FS_going_ro) {
            Some(Interrupt::Stopped)
        } else if !self.fs.reconcile_enabled() {
            Some(Interrupt::Disabled)
        } else if self.me.kicked_since(kick) {
            Some(Interrupt::Kicked)
        } else {
            None
        }
    }

    /// The scan phase's entry: a requested scan, done.
    fn scan_step(&self, p: &mut Pass<'_, 'f>, cookie: ScanCookie) -> Result<(), BchError> {
        if matches!(cookie.scan.get(), Ok(c::ReconcileScanRef::pending(_))) {
            p.pending = Some(cookie);
            // what retries pending work - a device added, resized, its state
            // or label changed
            self.fs.stripes_unpark();
        }

        match cookie.run(&self.ctxt, &mut p.w.s, &mut p.w.last_flushed) {
            Err(e) if is_restart(&e) => panic!("transaction restart out of a reconcile scan: {e}"),
            ret => p.sectors_scanned += ret?,
        }
        Ok(())
    }

    /// Walk keyed phase @phase until it's done or interrupted:
    /// do_reconcile_phase_iter(). A step that restarts, or waits for copygc,
    /// reads its entry again - but a buffered phase's, which it's taken.
    fn phase_walk(&self, p: &mut Pass<'_, 'f>, phase: &'static Phase) -> Result<(), BchError> {
        let fs = self.fs;
        let mut work_pos = c::bbpos { btree: phase.btree, pos: phase.start };

        while self.interrupted(p.kick).is_none() {
            let trans = self.ctxt.trans();
            // the last step's moves, or a wait for them, may have unlocked it
            trans.begin_raw();

            let ret = match phase.kind {
                PhaseKind::Scan => {
                    let next = next_key(trans, phase.btree, work_pos.pos, phase.end, |k| ScanCookie::from_key(fs, k))?;
                    let Some((at, cookie)) = next else { return Ok(()) };
                    work_pos.pos = at;
                    set_status(fs, Status::Running { phase: Some(phase), pos: at });
                    self.scan_step(p, cookie)
                }
                PhaseKind::Btree => {
                    // The backpointer is checked against its node's pointer in
                    // the attempt that read it, its reconcile_scan leaf still
                    // locked: a node rewrite deletes it there, and can't land
                    // in between
                    let t = trans.begin();
                    let next = next_key(t.trans(), phase.btree, work_pos.pos, phase.end, BkeyI::<c::Backpointer>::from_key)?;
                    let Some((at, bp)) = next else { return Ok(()) };
                    work_pos.pos = at;
                    set_status(fs, Status::Running { phase: Some(phase), pos: at });
                    p.w.btree(&t, work_pos, &bp)
                }
                PhaseKind::Normal => {
                    let Some(at) = p.w.work.next(trans, &mut work_pos, phase.end)? else { return Ok(()) };
                    work_pos.pos = at;
                    set_status(fs, Status::Running { phase: Some(phase), pos: at });
                    lockrestart_do(trans, |t| p.w.extent(t, work_pos))
                }
                PhaseKind::Phys => unreachable!("a phys phase is walked by its threads"),
            };

            match ret {
                Err(e) if e.matches(err::BCH_ERR_data_update_fail_need_copygc) => {
                    trans.unlock_long();
                    fs.copygc_wakeup();
                    p.copygc_runs = fs.copygc_wait_run(p.copygc_runs);
                    continue;
                }
                Err(e) if is_restart(&e) => continue,
                // the pass looks again, and waits
                Err(e) if e.matches(err::BCH_ERR_reconcile_scan_stop) => return Ok(()),
                Err(e) => return Err(e),
                Ok(()) => {}
            }

            let _ = p.w.retry_stripes();

            work_pos.pos = if work_pos.btree.has_snapshot_field() {
                work_pos.pos.successor()
            } else {
                work_pos.pos.nosnap_successor()
            };
        }
        Ok(())
    }

    /// Run phase @phase, from its start.
    fn phase(&self, p: &mut Pass<'_, 'f>, phase: &'static Phase) -> Result<(), BchError> {
        let fs = self.fs;
        set_status(fs, Status::Running { phase: Some(phase), pos: phase.start });

        if matches!(phase.kind, PhaseKind::Normal | PhaseKind::Phys) {
            fs.reconcile.progress.init_masks(fs, None, 1 << phase.btree.0, 0);
        }

        let _ = self.ctxt.trans().write_buffer_flush_sync();

        if phase.kind == PhaseKind::Phys {
            self.ctxt.trans().unlock_long();
            return reconcile_phys(fs, phase);
        }
        self.phase_walk(p, phase)
    }

    /// One pass through the phases - started over if it's kicked meanwhile -
    /// then a wait, if it found nothing to do: do_reconcile().
    fn pass(&self) -> Result<(), BchError> {
        let fs = self.fs;
        let mut p = Pass {
            w:               Walker::new(&self.ctxt)?,
            kick:            self.me.kicks(),
            pending:         None,
            sectors_scanned: 0,
            copygc_runs:     fs.copygc.run_count.get(),
        };

        let work_stats = MoveStats::from_c(&fs.reconcile.work_stats);
        work_stats.reset(c"reconcile_work");
        self.ctxt.flush_all();
        set_status(fs, Status::Running { phase: None, pos: POS_MIN });

        let ret = self.phases(&mut p);

        // Data that moved this pass - converted to EC, say - may have freed
        // the space a parked stripe needs. Parked stripes were skipped, so
        // this is never their own retries feeding each other
        if work_stats.sectors_moved() != 0 {
            fs.stripes_unpark();
        }

        if ret.is_ok() && !Kthread::should_stop() && work_stats.sectors_seen() == 0 &&
            p.sectors_scanned == 0 && !self.me.kicked_since(p.kick)
        {
            self.ctxt.flush_all();
            self.ctxt.trans().unlock_long();
            wait(fs, self.me, p.kick);
        }

        if let Err(e) = &ret {
            if !e.matches(c::EROFS) {
                bch_err!(fs, "do_reconcile(): error {e}");
            }
        }
        ret
    }

    /// The phases, until they're got through: completing the pending scan, if
    /// they ran its phases.
    fn phases(&self, p: &mut Pass<'_, 'f>) -> Result<(), BchError> {
        let fs = self.fs;

        loop {
            // a kick during the last try starts us over here, and what we
            // compare against after
            p.kick = self.me.kicks();

            let why = match self.interrupted(p.kick) {
                None => self.run_phases(p)?,
                why  => why,
            };

            match why {
                None => {
                    if let Some(cookie) = p.pending.take() {
                        cookie.complete(self.ctxt.trans())?;
                    }
                    set_status(fs, Status::Running { phase: None, pos: POS_MIN });
                    return Ok(());
                }
                Some(Interrupt::Stopped) => return Ok(()),
                Some(Interrupt::Disabled) => {
                    self.ctxt.flush_all();
                    let _ = sleep_until(Sleep::INTERRUPTIBLE.freezable(),
                                        || (fs.reconcile_enabled() || Kthread::should_stop()).then_some(()));
                    if Kthread::should_stop() {
                        return Ok(());
                    }
                }
                Some(Interrupt::Kicked) => {}
            }
        }
    }

    /// The phases, once through: what interrupted them, if anything did.
    fn run_phases(&self, p: &mut Pass<'_, 'f>) -> Result<Option<Interrupt>, BchError> {
        // copygc may have freed the buckets a parked stripe needed
        let runs = self.fs.copygc.run_count.get();
        if self.copygc_runs_unparked.replace(runs) != runs {
            self.fs.stripes_unpark();
        }

        for phase in &PHASES {
            // The pending phases run only when something requested a pending
            // scan - they're last, so that's the phases through
            if phase.is_pending() && p.pending.is_none() {
                return Ok(None);
            }

            self.phase(p, phase)?;
            p.w.work.clear();

            if let Some(why) = self.interrupted(p.kick) {
                return Ok(Some(why));
            }
            // what this phase started, done before the next
            self.ctxt.flush_all();
        }
        Ok(None)
    }
}

/// Whether there's high priority work - evacuating, rereplicating - which
/// doesn't wait.
fn hipri_work_pending(fs: &Fs) -> bool {
    let pos = c::disk_accounting_pos::from_arm(c::bch_acct_reconcile_work {
        type_: c::bch_reconcile_accounting_type::BCH_RECONCILE_ACCOUNTING_high_priority.0 as u8,
    });
    let mut v = [0; 2];
    accounting::mem_read(fs, &pos, &mut v);
    v != [0, 0]
}

/// Wait, after a pass with nothing to do: for the write io clock to reach a
/// sixty-fourth of the smallest RW device's capacity - 64MiB of writes, with
/// none - or a kick. reconcile_wait().
fn wait(fs: &Fs, me: &KthreadSelf<'_>, kick: u32) {
    if hipri_work_pending(fs) {
        cond_resched();
        return;
    }

    let clock = &fs.io_clock[WRITE];
    let now = clock.now.load(Relaxed) as u64;
    let capacity = match fs.min_rw_member_capacity() {
        u64::MAX => 128 * 2048,
        c => c,
    };
    let iotime_end = now + (capacity >> 6);

    let status = match fs.reconcile.status.read() {
        Status::Waiting { iotime_start, since, .. } => Status::Waiting { iotime_start, iotime_end, since },
        Status::Running { .. } => Status::Waiting { iotime_start: now, iotime_end, since: ktime_get_ns() },
    };
    set_status(fs, status);

    let _ = clock.wait_until(iotime_end, Sleep::INTERRUPTIBLE.freezable(),
                             || (me.kicked_since(kick) || Kthread::should_stop()).then_some(()));
}

// ── The thread ───────────────────────────────────────────────────────────

/// The reconcile thread: passes until it's stopped, or one fails.
fn thread(fs: &Fs, me: &KthreadSelf<'_>) -> Result<(), BchError> {
    Kthread::set_freezable();

    // Data moves can't run until check_snapshots has completed, and
    // snapshot_is_ancestor() is available - and until the logged ops we could
    // start ourselves (stripe creation) have been resumed, or recovery would
    // resume ours while we're running them.
    let ready = || fs.recovery.runner.pass_done.get() > c::bch_recovery_pass::BCH_RECOVERY_PASS_resume_logged_ops_early;
    let _ = sleep_until(Sleep::INTERRUPTIBLE.freezable(), || (ready() || Kthread::should_stop()).then_some(()));
    if Kthread::should_stop() {
        return Ok(());
    }

    let ctxt = MovingCtxt::new(fs, MoveStats::from_c(&fs.reconcile.work_stats),
                               writepoint_ptr(&fs.allocator.reconcile_write_point), true)?;
    let r = Reconciler { fs, me, ctxt, copygc_runs_unparked: Cell::new(0) };

    while !Kthread::should_stop() {
        r.pass()?;
    }
    Ok(())
}

impl Fs {
    /// Kick the reconcile thread: there's work.
    pub fn reconcile_wakeup(&self) {
        if let Some(t) = &*self.reconcile.thread.lock() {
            t.kick();
        }
    }

    /// Have reconcile scan for pending work, and kick it: as
    /// bch2_reconcile_pending_wakeup().
    pub fn reconcile_pending_wakeup(&self) -> Result<(), BchError> {
        self.request_scan(c::reconcile_scan::from_arm(c::reconcile_scan_pending {}), true)
    }

    /// Start the thread, unless it's running: bch2_reconcile_start().
    pub fn reconcile_start(&self) -> Result<(), BchError> {
        let r = &self.reconcile;

        // a thread that exited on its own is still ours to reap
        if r.thread_exit_ret.load(Relaxed) != 0 {
            self.reconcile_stop();
        }
        if r.thread.lock().is_some() || self.opts().nochanges != 0 {
            return Ok(());
        }

        r.thread_exit_ret.store(0, Relaxed);

        // SAFETY: the thread's stopped before the filesystem goes -
        // reconcile_stop(), on the way to read-only
        let fs: &'static Fs = unsafe { &*self.as_ptr() };
        let t = Kthread::create(format_args!("bch-reconcile/{}", self.name().to_str().unwrap_or("?")),
                                move |me| {
            if let Err(e) = thread(fs, me) {
                // going read-only isn't an error, as do_reconcile() has it
                if !e.matches(c::EROFS) {
                    fs.event_inc_trace(events::reconcile_error, |out| write!(out, "{e}"));
                }
                fs.reconcile.thread_exit_ret.store(e.raw(), Relaxed);
            }
        }).inspect_err(|e| bch_err!(self, "error creating reconcile thread: {e}"))?;

        r.thread.lock().insert(t).wake();
        Ok(())
    }

    /// Stop the thread, and wait for it to - outside the lock, which wakeups
    /// take: bch2_reconcile_stop().
    pub fn reconcile_stop(&self) {
        let t = self.reconcile.thread.lock().take();
        drop(t);
    }
}

// ── Status ───────────────────────────────────────────────────────────────

impl Fs {
    /// What reconcile is doing - and why it can't, without incompatible
    /// features: bch2_reconcile_status_to_text().
    #[cold]
    pub fn reconcile_status_to_text(&self, out: &mut Printbuf) {
        use c::bcachefs_metadata_version as v;
        out.tabstop_push(24);
        out.tabstop_push(12);
        out.tabstop_push(12);

        // Marking work needs incompatible features: without them every scan
        // completes, finds nothing to do, and the only other sign is a notice
        // once per boot (#1052, #946)
        let data_ok = self.incompat_feature_allowed(v::bcachefs_metadata_version_sb_field_extent_type_u64s);
        let meta_ok = self.incompat_feature_allowed(v::bcachefs_metadata_version_reconcile);
        if !data_ok || !meta_ok {
            let need = if data_ok { v::bcachefs_metadata_version_reconcile }
                       else { v::bcachefs_metadata_version_sb_field_extent_type_u64s };
            write!(out, "Reconcile disabled{}: needs incompatible feature ", if data_ok { " for metadata" } else { "" });
            need.to_text(out);
            write!(out, ", allowed up to ");
            c::bcachefs_metadata_version(self.sb.version_incompat_allowed.get().into()).to_text(out);
            write!(out, "\n  set version_upgrade=incompatible to enable\n\n");
        }

        match self.reconcile.status.read() {
            Status::Waiting { iotime_start, iotime_end, since } => {
                let now = self.io_clock[WRITE].now.load(Relaxed) as u64;
                write!(out, "waiting:\n");
                write!(out, "io wait duration:\t");
                out.human_readable_s64((iotime_end.wrapping_sub(iotime_start) << 9) as i64);
                write!(out, "\nio wait remaining:\t");
                out.human_readable_s64((iotime_end.wrapping_sub(now) << 9) as i64);
                write!(out, "\nduration waited:\t");
                out.time_units(ktime_get_ns().wrapping_sub(since));
                write!(out, "\n");
            }
            Status::Running { phase: None, .. } => write!(out, "between phases\n"),
            Status::Running { phase: Some(phase), pos } if phase.kind == PhaseKind::Scan => {
                write!(out, "scanning: ");
                let scan = c::reconcile_scan::from_cookie(pos.offset).unwrap_or_else(c::reconcile_scan::fs);
                scan.to_text(out, self);
                if matches!(scan.get(), Ok(c::ReconcileScanRef::fs(_) | c::ReconcileScanRef::metadata(_))) {
                    write!(out, " ");
                    self.reconcile.progress.to_text(out);
                }
                write!(out, "\n");
            }
            Status::Running { phase: Some(phase), pos } => {
                write!(out, "processing {} {}: ", phase.priority, phase.kind.name());
                if phase.kind == PhaseKind::Normal {
                    self.reconcile.progress.to_text(out);
                } else {
                    write!(out, "{pos}");
                }
                write!(out, "\n");
            }
        }

        write!(out, "\n");
        let exit_ret = self.reconcile.thread_exit_ret.load(Relaxed);
        // a ref of our own, to print it outside the lock: that allocates
        let task = self.reconcile.thread.lock().as_ref().map(|t| {
            // SAFETY: the Kthread's task, live while it's in the lock
            unsafe { c::get_task_struct(t.as_ptr()) };
            t.as_ptr()
        });

        if exit_ret != 0 {
            write!(out, "Reconcile thread exited: {}\n", BchError::from_raw(exit_ret));
        } else if let Some(task) = task {
            write!(out, "Reconcile thread backtrace:\n");
            out.indent_add(2);
            // SAFETY: our ref
            unsafe { c::bch2_prt_task_backtrace(out.as_raw(), task, 0, c::GFP_KERNEL) };
            out.indent_sub(2);
        } else {
            write!(out, "Reconcile thread not running\n");
        }

        if let Some(task) = task {
            // SAFETY: ours, from above
            unsafe { c::put_task_struct(task) };
        }
    }

    /// Whether a scan is requested: bch2_reconcile_scan_pending_to_text().
    #[cold]
    pub fn reconcile_scan_pending_to_text(&self, out: &mut Printbuf) {
        // No multithreaded btree access until BCH_FS_may_go_rw, and we're no
        // longer modifying the journal keys gap buffer
        if !self.flag(c::bch_fs_flags::BCH_FS_may_go_rw) {
            return;
        }

        let trans = crate::btree_trans!(self);
        let mut iter = BtreeIter::new(&trans, c::btree_id::reconcile_scan, POS_MIN, BtreeIterFlags::empty());
        let _ = lockrestart_do(&trans, |t| iter.peek(t).map(|_| ()));
        write!(out, "{}\n", u32::from(iter.pos().inode == 0));
    }
}

// ── Power ────────────────────────────────────────────────────────────────
//
// on_battery, for reconcile_on_ac_only: set as the filesystem starts, and
// by the power supply notifier on every change, which kicks reconcile.

#[cfg(CONFIG_POWER_SUPPLY)]
mod power {
    use super::*;

    fn on_battery() -> bool {
        // SAFETY: plain
        unsafe { c::power_supply_is_system_supplied() == 0 }
    }

    /// The power supply changed: bch2_reconcile_power_notifier().
    unsafe extern "C" fn notifier(nb: *mut c::notifier_block, _event: kernel::ffi::c_ulong,
                                  _data: *mut core::ffi::c_void) -> c_int {
        // SAFETY: our notifier block, in its filesystem
        let fs = unsafe {
            Fs::borrow_raw(nb.byte_sub(core::mem::offset_of!(c::bch_fs, reconcile.power_notifier)).cast())
        };
        fs.reconcile.on_battery.store(on_battery(), Relaxed);
        fs.reconcile_wakeup();
        c::NOTIFY_OK
    }

    pub(super) fn register(fs: &Fs) -> Result<(), BchError> {
        let nb = core::ptr::from_ref(&fs.reconcile.power_notifier).cast_mut();
        // SAFETY: ours, unregistered by unregister(); set up before it's
        // registered
        unsafe {
            (*nb).notifier_call = Some(notifier);
            crate::errcode::ret_to_result_void(c::power_supply_reg_notifier(nb))?;
        }
        fs.reconcile.on_battery.store(on_battery(), Relaxed);
        Ok(())
    }

    pub(super) fn unregister(fs: &Fs) {
        // SAFETY: ours, registered by register()
        unsafe { c::power_supply_unreg_notifier(core::ptr::from_ref(&fs.reconcile.power_notifier).cast_mut()) };
    }
}

/// No power supplies to tell: never on battery.
#[cfg(not(CONFIG_POWER_SUPPLY))]
mod power {
    use super::*;

    pub(super) fn register(_: &Fs) -> Result<(), BchError> {
        Ok(())
    }

    pub(super) fn unregister(_: &Fs) {}
}

// ── C's ──────────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_reconcile_wakeup(c: &Fs) {
    c.reconcile_wakeup()
}

#[c_export]
pub fn bch2_reconcile_start(c: &Fs) -> c_int {
    ret_to_c(c.reconcile_start())
}

#[c_export]
pub fn bch2_reconcile_stop(c: &Fs) {
    c.reconcile_stop()
}

#[c_export]
pub fn bch2_fs_reconcile_exit(c: &Fs) {
    power::unregister(c)
}

#[c_export]
pub fn bch2_fs_reconcile_init(c: &Fs) -> c_int {
    ret_to_c(power::register(c))
}
