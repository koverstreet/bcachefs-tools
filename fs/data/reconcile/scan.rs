// SPDX-License-Identifier: GPL-2.0

//! Reconcile scans: work the work btrees can't find - every extent, the
//! metadata, a device's, an inode's, the stripes, the pending extents -
//! requested by option and device changes, and done by the reconcile thread.
//!
//! A requested scan is a cookie: a KEY_TYPE_cookie in BTREE_ID_reconcile_scan,
//! at the scan's cookie_pos(), its value a generation. Requesting bumps it.
//! The thread reads it - a ScanCookie - does the scan, then complete()s it:
//! deletes it only if the generation hasn't moved, so a scan requested again
//! meanwhile, for a change the pass may have missed, is done again.
//!
//! Positions are pos(0, n): fs 0, metadata 1, pending 2, stripes 3, a device
//! 32 + its index, an inode its number, BCACHEFS_ROOT_INO up. Nonzero inodes
//! are reconcile's btree node backpointers. One of 4..31 is a scan type this
//! version doesn't know - a newer one's - and is done as an fs scan, which
//! covers anything narrower: a downgrade loses no scan.

use crate::alloc::backpointers::{backpointer_get_key, BpScanIter};
use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bpos_ge, pos, BkeyI, BkeySC, POS_MIN};
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeLevels, BtreeTrans,
                         CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::btree::write_buffer::{WbFlush, WbMaybeFlush};
use crate::c;
use crate::data::ec::create::WidenCache;
use crate::data::moving::MovingCtxt;
use crate::data::reconcile::needs_reconcile::update_reconcile_opts;
use crate::errcode::{ret_to_c, BchError};
use crate::namei::inum_snapshot_to_path;
use crate::fs::Fs;
use crate::inode_opts::{get_io_opts, PerSnapshotIoOpts};
use crate::locking::Lock;
use crate::sb::counters::events;
use crate::util::kthread::Kthread;
use crate::util::Printbuf;
use crate::{bch_err, SPOS_MAX};
use core::ffi::c_int;
use core::ops::Range;
use cstruct_macros::c_export;

/// The scan types' names, by type: C's bch2_rebalance_scan_strs[].
macro_rules! scan_type_strs {
    ($(($n:ident)),* $(,)?) => { [$(stringify!($n)),*] };
}
const SCAN_TYPE_STRS: [&str; 6] = c::RECONCILE_SCAN_TYPES!(scan_type_strs);

impl c::reconcile_scan {
    /// The scan for everything.
    pub fn fs() -> Self {
        Self::from_arm(c::reconcile_scan_fs {})
    }

    /// Its cookie's offset in its position: the encoding.
    fn cookie(&self) -> u64 {
        use c::ReconcileScanRef as S;
        match self.get().expect("a scan of a known type") {
            S::fs(_)       => c::RECONCILE_SCAN_COOKIE_fs.into(),
            S::metadata(_) => c::RECONCILE_SCAN_COOKIE_metadata.into(),
            S::pending(_)  => c::RECONCILE_SCAN_COOKIE_pending.into(),
            S::stripes(_)  => c::RECONCILE_SCAN_COOKIE_stripes.into(),
            S::device(&d)  => u64::from(c::RECONCILE_SCAN_COOKIE_device + d),
            S::inum(&i)    => i,
        }
    }

    /// Where its cookie is, in BTREE_ID_reconcile_scan.
    pub fn cookie_pos(&self) -> c::bpos {
        pos(0, self.cookie())
    }

    /// The scan whose cookie is @cookie - None for a type this version
    /// doesn't know.
    pub fn from_cookie(cookie: u64) -> Option<Self> {
        Some(match cookie {
            v if v >= c::BCACHEFS_ROOT_INO.into() => Self::from_arm::<c::reconcile_scan_inum>(v),
            v if v >= c::RECONCILE_SCAN_COOKIE_device.into() =>
                Self::from_arm::<c::reconcile_scan_device>((v - u64::from(c::RECONCILE_SCAN_COOKIE_device)) as u32),
            v if v == c::RECONCILE_SCAN_COOKIE_pending.into()  => Self::from_arm(c::reconcile_scan_pending {}),
            v if v == c::RECONCILE_SCAN_COOKIE_stripes.into()  => Self::from_arm(c::reconcile_scan_stripes {}),
            v if v == c::RECONCILE_SCAN_COOKIE_metadata.into() => Self::from_arm(c::reconcile_scan_metadata {}),
            v if v == c::RECONCILE_SCAN_COOKIE_fs.into()       => Self::fs(),
            _ => return None,
        })
    }

    /// The scan whose cookie is @cookie: one of a type this version doesn't
    /// know, reported, is an fs scan - see the top of the file.
    fn at_cookie(fs: &Fs, cookie: u64) -> Self {
        Self::from_cookie(cookie).unwrap_or_else(|| {
            bch_err!(fs, "unknown reconcile scan cookie {cookie}");
            Self::fs()
        })
    }

    /// Its type, and which device - by name - or inode - by path, looked up
    /// in a transaction of its own.
    pub fn to_text(&self, out: &mut Printbuf, fs: &Fs) {
        write!(out, "{}", SCAN_TYPE_STRS[self.type_().into_bits() as usize]);
        match self.get() {
            Ok(c::ReconcileScanRef::device(&d)) => {
                write!(out, ": ");
                fs.member_name_to_text(out, d);
            }
            Ok(c::ReconcileScanRef::inum(&i)) => {
                write!(out, ": ");
                let trans = crate::btree_trans!(fs);
                let _ = lockrestart_do(&trans, |t| {
                    let restore = out.state_save();
                    inum_snapshot_to_path(t, i, 0, out).inspect_err(|_| out.state_restore(restore))
                });
            }
            _ => {}
        }
    }
}

impl TransAttempt<'_, '_> {
    /// Request @scan: its cookie's generation bumped, queued in the attempt.
    pub fn request_scan(&self, scan: c::reconcile_scan) -> Result<(), BchError> {
        let p = scan.cookie_pos();
        let mut iter = BtreeIter::new(self, c::btree_id::reconcile_scan, p, BtreeIterFlags::INTENT);
        let k = iter.peek_slot(self)?.expect("a slot");

        let mut n = BkeyI::<c::Cookie>::new();
        n.k.p = p;
        n.v.cookie = (cookie_generation(k) + 1).into();
        let n = self.bkey_copy(n.as_ref())?;
        self.update(&iter, &n, UpdateTriggerFlags::empty())
    }

    /// Whether @scan is requested: its cookie's there.
    pub fn scan_requested(&self, scan: c::reconcile_scan) -> Result<bool, BchError> {
        let mut iter = BtreeIter::new(self, c::btree_id::reconcile_scan, scan.cookie_pos(), BtreeIterFlags::empty());
        Ok(iter.peek_slot(self)?.expect("a slot").arm::<c::Cookie>().is_some())
    }
}

impl Fs {
    /// Request @scan, committed - and kick reconcile, if @wakeup.
    pub fn request_scan(&self, scan: c::reconcile_scan, wakeup: bool) -> Result<(), BchError> {
        let trans = crate::btree_trans!(self);
        commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| t.request_scan(scan))?;
        if wakeup {
            self.reconcile_wakeup();
        }
        Ok(())
    }
}

/// A cookie's generation: 0 for none.
fn cookie_generation(k: BkeySC<'_>) -> u64 {
    k.arm::<c::Cookie>().map_or(0, |c| c.v().cookie.get())
}

/// A requested scan as the reconcile thread found it: its cookie, at the
/// generation it was then.
#[derive(Clone, Copy)]
pub struct ScanCookie {
    pub scan:   c::reconcile_scan,
    pos:        c::bpos,
    generation: u64,
}

impl ScanCookie {
    /// Cookie key @k's - None for a key that isn't one.
    pub fn from_key(fs: &Fs, k: BkeySC<'_>) -> Option<Self> {
        let generation = k.arm::<c::Cookie>()?.v().cookie.get();
        Some(ScanCookie { scan: c::reconcile_scan::at_cookie(fs, k.k.p.offset), pos: k.k.p, generation })
    }

    /// The scan's done: delete its cookie, unless it's been requested again
    /// since - whether it was.
    pub fn complete(self, trans: &BtreeTrans<'_>) -> Result<bool, BchError> {
        let fs = trans.fs();
        let mut now = 0;
        commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| {
            let mut iter = BtreeIter::new(t, c::btree_id::reconcile_scan, self.pos, BtreeIterFlags::INTENT);
            now = cookie_generation(iter.peek_slot(t)?.expect("a slot"));
            if now == self.generation {
                t.delete_at(&iter, UpdateTriggerFlags::empty())?;
            }
            Ok(())
        })?;

        let deleted = now == self.generation;
        fs.event_inc_trace(events::reconcile_clear_scan, |out| {
            self.scan.to_text(out, fs);
            write!(out, "\nscan started with cookie {} now have {}\n", self.generation, now);
            write!(out, "{}deleting scan cookie\n", if deleted { "" } else { "not " });
        });
        Ok(deleted)
    }
}

// ── Option changes in flight ─────────────────────────────────────────────
//
// An option change brackets itself with requests - pre and post - so writers'
// triggers re-derive their keys' reconcile state as the new option lands. A
// pass that scans while the change is half applied must not complete the
// cookie: on the error path nothing requests it again. So pre registers the
// cookie, then requests it, and the reconcile thread doesn't start a scan
// whose cookie is registered; a pass that started before can't complete it
// either, the request having moved its generation. The registrations are the
// change's, dropped when it's done (OptChange).
//
// One change at a time - each holds opt_change_lock - so they're the cookies
// of that one, OPT_CHANGE_SCANS_MAX at most; one cookie twice, from a
// change that requests it for two options, is two.

/// The most scans one option change requests.
const OPT_CHANGE_SCANS_MAX: usize = 4;

/// The scans the option change in progress has requested, by cookie.
pub struct OptChangeScans {
    nr:      usize,
    cookies: [u64; OPT_CHANGE_SCANS_MAX],
}

impl OptChangeScans {
    pub const fn new() -> Self {
        OptChangeScans { nr: 0, cookies: [0; OPT_CHANGE_SCANS_MAX] }
    }
}

impl Fs {
    /// An option change is about to change what @scan covers: registered,
    /// then requested, so writers re-derive against the new option as it
    /// lands.
    pub fn opt_change_scan_pre(&self, scan: c::reconcile_scan) -> Result<(), BchError> {
        Lock::from_c(&self.opt_change_lock).assert_held();
        {
            let mut s = self.reconcile.opt_change_scans.lock();
            assert!(s.nr < OPT_CHANGE_SCANS_MAX, "an option change requesting more than {OPT_CHANGE_SCANS_MAX} scans");
            let nr = s.nr;
            s.cookies[nr] = scan.cookie();
            s.nr += 1;
        }
        let trans = crate::btree_trans!(self);
        commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| t.request_scan(scan))
    }

    /// The option change has landed: @scan requested again, now against the
    /// new option, and reconcile kicked - even if the request failed. Its
    /// registration is dropped with the change, not here.
    pub fn opt_change_scan_post(&self, scan: c::reconcile_scan) -> Result<(), BchError> {
        let trans = crate::btree_trans!(self);
        let ret = commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| t.request_scan(scan));
        self.reconcile_wakeup();
        ret
    }

    /// The option change is done, landed or not: OptChange's drop.
    pub fn opt_change_scans_done(&self) {
        self.reconcile.opt_change_scans.lock().nr = 0;
    }

    /// Whether the option change in progress has requested the scan of
    /// @cookie: the reconcile thread doesn't start it.
    pub fn scan_in_flight(&self, cookie: u64) -> bool {
        let s = self.reconcile.opt_change_scans.lock();
        s.cookies[..s.nr].contains(&cookie)
    }
}

// ── Doing them ───────────────────────────────────────────────────────────
//
// A scan brings each key it covers in line with its options -
// update_reconcile_opts(), which queues the work the key now needs - in an
// attempt per key, committed. It stops (reconcile_scan_stop) if the thread's
// told to or reconcile's been disabled, its cookie left for the next pass to
// start again.
//
// Each scan counts what it saw in the scan stats, and sets their position as
// it goes, for the moving contexts' listing - the pass made them the
// context's.

const NO_ENOSPC: CommitFlags = CommitFlags::NO_ENOSPC;

fn stop(fs: &Fs) -> Result<(), BchError> {
    if Kthread::should_stop() || !fs.reconcile_enabled() {
        return Err(c::bch_errcode::BCH_ERR_reconcile_scan_stop.into());
    }
    Ok(())
}

impl ScanCookie {
    /// Do the scan, then complete it - not a pending scan, which the pass
    /// completes after its pending phases. The sectors it saw, plus one, so a
    /// pass that made work for the next doesn't wait - none if it didn't run.
    ///
    /// Not a scan the option change in progress has requested: now it would
    /// see the change half applied, and could complete the cookie - so not
    /// until it's settled, and opt_change_scan_post()'s wakeup brings the
    /// thread back.
    pub fn run<'f>(self, ctxt: &MovingCtxt<'f>, s: &mut PerSnapshotIoOpts, last_flushed: &mut WbMaybeFlush)
        -> Result<u64, BchError>
    {
        use c::ReconcileScanRef as S;
        let fs: &'f Fs = ctxt.trans().fs();

        if fs.scan_in_flight(self.pos.offset) {
            return Ok(0);
        }

        let stats = &fs.reconcile.scan_stats;
        stats.reset(c"reconcile_scan");
        ctxt.set_stats(stats);

        let scan = &self.scan;
        match scan.get() {
            Ok(S::fs(_))       => scan_fs(ctxt, scan, s, false)?,
            Ok(S::metadata(_)) => scan_fs(ctxt, scan, s, true)?,
            Ok(S::device(&d))  => scan_device(ctxt, scan, d, last_flushed)?,
            Ok(S::stripes(_))  => scan_stripes(ctxt)?,
            Ok(S::inum(&i))    => scan_btree(ctxt, scan, s, c::btree_id::extents, 0..1,
                                             pos(i, 0), pos(i, u64::MAX))?,
            Ok(S::pending(_)) | Err(_) => {}
        }

        if !matches!(scan.get(), Ok(S::pending(_))) {
            self.complete(ctxt.trans())?;
        }
        Ok(stats.sectors_seen() + 1)
    }
}

/// Every btree's node pointers, and with @metadata unset the extents' and
/// reflink btrees' leaves.
fn scan_fs(ctxt: &MovingCtxt<'_>, scan: &c::reconcile_scan, s: &mut PerSnapshotIoOpts, metadata: bool)
    -> Result<(), BchError>
{
    let fs = ctxt.trans().fs();
    fs.reconcile.progress.init_masks(fs, None, if metadata { 0 } else { !0 }, !0);

    for btree in 0..fs.btree_id_nr_alive() {
        if fs.btree_id_root(btree).is_none() {
            continue;
        }
        let btree = c::btree_id(btree);
        let leaves = !metadata && (btree == c::btree_id::extents || btree == c::btree_id::reflink);
        scan_btree(ctxt, scan, s, btree, u32::from(!leaves)..c::BTREE_MAX_DEPTH, POS_MIN, SPOS_MAX)?;
    }
    Ok(())
}

/// @btree's keys at @levels, from @start to @end. An inode's - a scan of the
/// extents from a nonzero inode - follows its reflink pointers that may
/// update options, to the indirect extents they share.
fn scan_btree(ctxt: &MovingCtxt<'_>, scan: &c::reconcile_scan, s: &mut PerSnapshotIoOpts, btree: c::btree_id,
              levels: Range<u32>, start: c::bpos, end: c::bpos) -> Result<(), BchError>
{
    let trans = ctxt.trans();
    let fs = trans.fs();
    let res = DiskReservation::new(fs);

    BtreeLevels { btree, levels, start, end }.for_each_commit(trans, Some(&res), NO_ENOSPC, |t, iter, level, k| {
        ctxt.stats().set_pos(c::bbpos { btree: iter.btree(), pos: iter.pos() });
        fs.reconcile.progress.update(t, iter)?;
        fs.reconcile.scan_stats.add_sectors_seen(if level == 0 { k.k.size.into() } else { fs.btree_sectors().into() });
        res.put();
        stop(fs)?;

        let opts = get_io_opts(t, Some(&mut **s), k)?;
        update_opts(t, Some(&mut **s), &opts, iter, level, k, scan)?;

        if let Some(refs) = k.arm::<c::ReflinkP>()
            .filter(|p| start.inode != 0 && p.v().may_update_options())
            .map(|p| p.refd()) {
            t.ignore_restarts(|trans| scan_indirect(trans, scan, &res, refs, s, &opts))?;
        }
        Ok(())
    })
}

/// The indirect extents a reflink pointer holds references on, @refs, given
/// @opts - its inode's: do_reconcile_scan_indirect().
fn scan_indirect(trans: &BtreeTrans<'_>, scan: &c::reconcile_scan, res: &DiskReservation<'_>, refs: Range<u64>,
                 s: &mut PerSnapshotIoOpts, opts: &c::bch_inode_opts) -> Result<(), BchError>
{
    let mut iter = BtreeIter::new(trans, c::btree_id::reflink, pos(0, refs.start),
                                  BtreeIterFlags::INTENT | BtreeIterFlags::NOT_EXTENTS);
    iter.for_each_commit(trans, Some(res), NO_ENOSPC, |t, iter, k| {
        if bpos_ge(k.k.start_pos(), pos(0, refs.end)) {
            return Err(c::bch_errcode::BCH_ERR_fc_break.into());
        }
        res.put();
        update_opts(t, Some(&mut **s), opts, iter, 0, k, scan)
    })
}

/// Device @dev's data, through its backpointers: what a change to its state
/// or durability changes.
fn scan_device(ctxt: &MovingCtxt<'_>, scan: &c::reconcile_scan, dev: u32, last_flushed: &mut WbMaybeFlush)
    -> Result<(), BchError>
{
    let trans = ctxt.trans();
    let fs = trans.fs();
    let mut bps = BpScanIter::new(c::btree_id::backpointers, pos(dev.into(), 0), None);

    bps.for_each(trans, pos(dev.into(), u64::MAX), last_flushed.borrow(), |t, bp, last_flushed| {
        ctxt.stats().set_pos(c::bbpos { btree: c::btree_id::backpointers, pos: bp.k.p });
        stop(fs)?;
        scan_bp(t, scan, bp, last_flushed)?;
        t.commit(Some(&DiskReservation::new(fs)), NO_ENOSPC)
    })
}

/// What backpointer @bp points to - not an erasure coded extent: it's
/// handled at its stripe.
fn scan_bp(t: &TransAttempt<'_, '_>, scan: &c::reconcile_scan, bp: BkeySC<'_, c::Backpointer>,
           last_flushed: WbFlush<'_>) -> Result<(), BchError>
{
    if bp.v().erasure_coded() {
        return Ok(());
    }

    let fs = t.fs();
    let level = bp.v().level.into();
    let mut iter = BtreeIter::uninit();
    let Some((iter, k)) = backpointer_get_key(t, bp, &mut iter, BtreeIterFlags::INTENT, last_flushed)? else {
        return Ok(());
    };

    fs.reconcile.scan_stats.add_sectors_seen(if level == 0 { k.k.size.into() } else { fs.btree_sectors().into() });
    let opts = get_io_opts(t, None, k)?;
    update_opts(t, None, &opts, iter, level, k, scan)
}

/// The stripes' can_widen, against the RW members their disk labels have
/// now - what a device's state changing changes.
fn scan_stripes(ctxt: &MovingCtxt<'_>) -> Result<(), BchError> {
    let trans = ctxt.trans();
    let fs = trans.fs();
    let mut cache = WidenCache::new()?;
    fs.reconcile.progress.init_masks(fs, None, 1 << c::btree_id::stripes.0, 0);

    let mut iter = BtreeIter::new(trans, c::btree_id::stripes, POS_MIN, BtreeIterFlags::PREFETCH);
    iter.for_each_commit(trans, None, NO_ENOSPC, |t, iter, k| {
        ctxt.stats().set_pos(c::bbpos { btree: iter.btree(), pos: iter.pos() });
        fs.reconcile.progress.update(t, iter)?;
        fs.reconcile.scan_stats.add_sectors_seen(fs.btree_sectors().into());
        stop(fs)?;

        let Some(s) = k.arm::<c::Stripe>() else { return Ok(()) };
        let want = cache.can_widen(fs, s.v())?;
        if s.v().can_widen() != want {
            let mut u = t.bkey_make_mut(iter, s, UpdateTriggerFlags::empty())?;
            u.v_mut().bits.set_can_widen(want);
        }
        Ok(())
    })
}

/// update_reconcile_opts(), for a scan: traced as the scan's.
fn update_opts(t: &TransAttempt<'_, '_>, s: Option<&mut c::per_snapshot_io_opts>, opts: &c::bch_inode_opts,
               iter: &BtreeIter<'_>, level: u32, k: BkeySC<'_>, scan: &c::reconcile_scan) -> Result<(), BchError>
{
    let fs = t.fs();
    let sectors = if level == 0 { k.k.size.into() } else { fs.btree_sectors().into() };
    let msg = |out: &mut Printbuf| write!(out, "{}", k.to_text(fs));

    macro_rules! trace {
        ($(($n:ident)),* $(,)?) => { crate::paste! {
            match scan.type_() {
                $(c::reconcile_scan_type::[<RECONCILE_SCAN_ $n>] =>
                    fs.event_add_trace(events::[<reconcile_scan_ $n>], sectors, msg),)*
            }
        } };
    }
    c::RECONCILE_SCAN_TYPES!(trace);

    update_reconcile_opts(t, s, opts, iter, level, k, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_opt_change)
}

// ── C's ──────────────────────────────────────────────────────────────────
//
// opts.c's and dev.c's, until they're Rust.

#[c_export]
pub fn bch2_set_reconcile_needs_scan_pre(c: &Fs, s: c::reconcile_scan) -> c_int {
    ret_to_c(c.opt_change_scan_pre(s))
}

#[c_export]
pub fn bch2_set_reconcile_needs_scan_post(c: &Fs, s: c::reconcile_scan) -> c_int {
    ret_to_c(c.opt_change_scan_post(s))
}

#[c_export]
pub fn bch2_set_reconcile_needs_scan(c: &Fs, s: c::reconcile_scan, wakeup: bool) -> c_int {
    ret_to_c(c.request_scan(s, wakeup))
}

/// The set_fs_needs_reconcile recovery pass, once, upgrading from
/// rebalance_work: an fs scan, which gives every extent its reconcile state.
#[c_export]
pub fn bch2_set_fs_needs_reconcile(c: &Fs) -> c_int {
    ret_to_c(c.request_scan(c::reconcile_scan::fs(), true))
}
