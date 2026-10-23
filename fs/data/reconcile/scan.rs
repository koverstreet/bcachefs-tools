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

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeySC};
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
                         TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::namei::inum_snapshot_to_path;
use crate::fs::Fs;
use crate::locking::Lock;
use crate::sb::counters::events;
use crate::util::Printbuf;
use crate::bch_err;
use core::ffi::c_int;
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

// ── C's, until work.c converts ───────────────────────────────────────────

#[c_export]
pub fn bch2_set_reconcile_needs_scan_pre(c: &Fs, s: c::reconcile_scan) -> c_int {
    ret_to_c(c.opt_change_scan_pre(s))
}

#[c_export]
pub fn bch2_set_reconcile_needs_scan_post(c: &Fs, s: c::reconcile_scan) -> c_int {
    ret_to_c(c.opt_change_scan_post(s))
}

#[c_export]
pub fn bch2_reconcile_scan_in_flight(c: &Fs, cookie: u64) -> bool {
    c.scan_in_flight(cookie)
}

#[c_export]
pub fn bch2_reconcile_scan_decode(c: &Fs, cookie: u64) -> c::reconcile_scan {
    c::reconcile_scan::at_cookie(c, cookie)
}

#[cold]
#[c_export]
pub fn bch2_reconcile_scan_to_text(out: &mut Printbuf, c: &Fs, s: c::reconcile_scan) {
    s.to_text(out, c)
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

/// The scan at @pos, started at @generation, done: ScanCookie::complete().
#[c_export]
pub fn bch2_reconcile_scan_complete(trans: &BtreeTrans<'_>, pos: c::bpos, generation: u64) -> c_int {
    let fs = trans.fs();
    let cookie = ScanCookie { scan: c::reconcile_scan::at_cookie(fs, pos.offset), pos, generation };
    ret_to_c(cookie.complete(trans).map(|_| ()))
}
