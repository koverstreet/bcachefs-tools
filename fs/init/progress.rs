// SPDX-License-Identifier: GPL-2.0

//! Progress indicators (init/progress.h), for work that walks btrees or
//! counts its own units: printed to dmesg every ten seconds as the work goes,
//! for work with nowhere better to report - mount - and read out meanwhile
//! by sysfs and BCH_IOCTL_RECOVERY_STATUS.
//!
//! An indicator is wherever its work keeps it: c->recovery.progress, which
//! recovery passes share - Progress::recovery() - reconcile's, snapshot
//! deletion's, a local. Its state is a SeqLock's: the work is its one writer,
//! updating it as it goes, and readers copy it out whole, never half an
//! update - which C's unlocked reads of the same fields could get. C goes
//! through the functions here, C's API as it was, and
//! bch2_progress_status() for what the ioctl copies out.
//!
//! A zeroed indicator is a valid one, never initialized - as bch_fs embeds
//! them, allocated zeroed; a C local is `= {}`.

use crate::btree::iter::{BtreeIter, BtreeTrans, TransAttempt};
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::passes;
use crate::util::ffi::{c_char, Opaque};
use crate::util::kernel::local_clock;
use crate::util::{Printbuf, ToText};
use core::ffi::CStr;
use cstruct_macros::c_export;

crate::module_param! {
    /// For testing the mount-time status display: a filesystem that
    /// recovers slowly enough to watch has to be enormous, so stretch a
    /// small one instead. Zero, and inert, unless set.
    ///
    /// This sleeps with btree locks held. That is survivable only because
    /// bch2_trans_begin() takes them back off us on the next iteration - past
    /// 10ms of srcu it calls bch2_trans_unlock_long(), and past
    /// BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS it calls bch2_trans_unlock() and
    /// reschedules. Nothing here is a model for real code.
    pub(crate) static recovery_progress_delay_ms: u32 = 0;
    perm = 0o644, desc = "testing only: sleep this long in every recovery progress update";
}

fn maybe_delay() {
    let ms = recovery_progress_delay_ms.get();
    if ms != 0 {
        crate::util::kernel::msleep(ms);
    }
}

/// How often an indicator prints, in local_clock()'s nanoseconds.
const PRINT_INTERVAL: u64 = 10 * 1_000_000_000;

/// BCH_PROGRESS_UNITS()'s names, by value.
macro_rules! progress_units {
    ($(($name:tt, $value:expr $(, $($rest:tt)*)?)),* $(,)?) => {
        const UNITS: &[(&str, u32)] = &[$((stringify!($name), $value),)*];
    };
}
crate::c::BCH_PROGRESS_UNITS!(progress_units);

fn units_str(units: c::bch_progress_units) -> &'static str {
    UNITS.iter().find(|&&(_, v)| v == units.0).map_or("(unknown units)", |&(n, _)| n)
}

/// What an indicator says - copied out of the SeqLock whole.
#[derive(Clone, Copy, ::pin_init::Zeroable)]
pub(crate) struct ProgressState {
    msg:        Option<&'static CStr>,
    units:      c::bch_progress_units,
    pos:        c::bbpos,
    /// local_clock() at which it's next due to print
    next_print: u64,
    seen:       u64,
    total:      u64,
    /// The last node the walk was in, by address - only ever compared
    last_node:  usize,
    silent:     bool,
}

impl ProgressState {
    fn new(msg: Option<&'static CStr>, units: c::bch_progress_units, total: u64) -> Self {
        Self {
            msg: msg.map(strip_bch2),
            units,
            next_print: local_clock() + PRINT_INTERVAL,
            total,
            ..::pin_init::Zeroable::zeroed()
        }
    }

    /// Whether to print now - and if so, not again for PRINT_INTERVAL.
    fn print_due(&mut self, fs: &Fs) -> bool {
        // Progress decides this for itself instead of leaving it to the stdio
        // redirect: bch_info() sends log traffic to dmesg whenever the redirect
        // is user-only, which is exactly the case where a mount is reading
        // progress out of BCH_IOCTL_RECOVERY_STATUS and drawing it. Checked
        // ahead of the interval, which would otherwise be eaten - the one we
        // want to print on as soon as the reader goes away.
        if fs.stdio_progress_reader() || self.silent || self.msg.is_none() {
            return false;
        }

        let now = local_clock();
        if now < self.next_print {
            return false;
        }
        self.next_print = now + PRINT_INTERVAL;
        true
    }
}

/// How far it's got: as bch2_progress_to_text().
impl ToText for ProgressState {
    fn to_text(&self, out: &mut Printbuf) {
        let percent = if self.total != 0 {
            (self.seen.wrapping_mul(100) / self.total) as u32
        } else {
            0
        };
        write!(out, "{percent}%, done {}/{} {}", self.seen, self.total, units_str(self.units));

        // No node means no position: a counter-based indicator, or nothing seen yet
        if self.last_node != 0 {
            crate::prt!(out, ", at {}", self.pos);
        }
    }
}

/// C's __func__ for a message: without its bch2_ prefix.
fn strip_bch2(msg: &'static CStr) -> &'static CStr {
    msg.to_bytes_with_nul()
        .strip_prefix(b"bch2_")
        .and_then(|rest| CStr::from_bytes_with_nul(rest).ok())
        .unwrap_or(msg)
}

fn btree_mask(ids: &[c::btree_id]) -> u64 {
    ids.iter().fold(0u64, |m, &id| m | 1 << id.0 as u64)
}

/// A walk's total, in nodes - @leaf_mask's btrees' leaves and @inner_mask's
/// interior nodes - estimated from accounting.
fn total_nodes(fs: &Fs, leaf_mask: u64, inner_mask: u64) -> u64 {
    // This is only an estimation: nodes can have different replica counts
    let expected_node_disk_sectors =
        fs.opts().metadata_replicas as u64 * (fs.opts().btree_node_size as u64 >> 9);

    let mut total = 0u64;
    // The masks have a bit for each of the first 64 btrees
    for i in 0..fs.btree_id_nr_alive().min(64) {
        let bit = 1u64 << i;
        if (leaf_mask | inner_mask) & bit == 0 {
            continue;
        }

        // disk_sectors, total_nodes, inner_nodes
        let mut v = [0u64; 3];
        crate::accounting::mem_read(fs, &c::disk_accounting_pos::from_arm(c::bch_acct_btree { id: i }),
                                    &mut v);
        let [disk_sectors, total_nodes, inner_nodes] = v;

        // Better to estimate as 0 than the total node count
        if inner_mask & bit != 0 {
            total += inner_nodes;
        }

        if leaf_mask & bit == 0 {
            continue;
        }

        // We check for zeros to degrade gracefully when run with
        // un-upgraded accounting info (missing some counters).
        total += if total_nodes != 0 {
            total_nodes.wrapping_sub(inner_nodes)
        } else {
            disk_sectors / expected_node_disk_sectors
        };
    }
    total
}

impl c::progress_indicator {
    /// Start reporting a walk of @leaf_btrees' keys (and @inner_btrees'
    /// interior nodes), as @msg: as bch2_progress_init(). C passes __func__
    /// as @msg.
    pub fn init(
        &self,
        fs:           &Fs,
        msg:          &'static CStr,
        leaf_btrees:  &[c::btree_id],
        inner_btrees: &[c::btree_id],
    ) {
        self.init_masks(fs, Some(msg), btree_mask(leaf_btrees), btree_mask(inner_btrees))
    }

    /// init(), the btrees as masks - a bit per btree id.
    pub(crate) fn init_mask(&self, fs: &Fs, msg: &'static CStr, leaf_mask: u64, inner_mask: u64) {
        self.init_masks(fs, Some(msg), leaf_mask, inner_mask)
    }

    fn init_masks(&self, fs: &Fs, msg: Option<&'static CStr>, leaf_mask: u64, inner_mask: u64) {
        let total = total_nodes(fs, leaf_mask, inner_mask);
        let new = ProgressState::new(msg, c::bch_progress_units::BCH_PROGRESS_UNITS_nodes, total);
        self.state.write(|s| *s = new);
    }

    /// Start reporting work that isn't a btree walk - no node to count, no
    /// position - of @total @units, counted by update_count().
    pub fn init_count(&self, msg: Option<&'static CStr>, units: c::bch_progress_units, total: u64) {
        let new = ProgressState::new(msg, units, total);
        self.state.write(|s| *s = new);
    }

    /// Nothing in dmesg, only sysfs and the ioctl.
    pub fn set_silent(&self) {
        self.state.write(|s| s.silent = true);
    }

    /// Back to nothing seen, nothing said.
    pub fn reset(&self) {
        self.state.write(|s| *s = ::pin_init::Zeroable::zeroed());
    }

    /// Report the walk reaching @iter's position: as bch2_progress_update_iter().
    pub fn update<'a, 't>(&self, t: &TransAttempt<'a, 't>, iter: &BtreeIter<'t>) -> Result<(), BchError> {
        let fs = t.fs();

        passes::cancelled(fs)?;
        maybe_delay();

        let Some(b) = iter.node_at_iter_level(t) else {
            return Ok(());
        };
        let pos = c::bbpos { btree: b.btree_id(), pos: b.key().k.p };

        let print = self.state.write(|s| {
            s.seen += (b.addr() != s.last_node && pos > s.pos) as u64;
            s.last_node = b.addr();
            s.pos = pos;
            s.print_due(fs)
        });
        if print {
            self.print(fs);
        }
        Ok(())
    }

    /// Count one of an init_count() indicator's units: as
    /// bch2_progress_update_count().
    pub fn update_count(&self, fs: &Fs) {
        maybe_delay();

        let print = self.state.write(|s| {
            s.seen += 1;
            s.print_due(fs)
        });
        if print {
            self.print(fs);
        }
    }

    fn print(&self, fs: &Fs) {
        let s = self.state.read();
        if let Some(msg) = s.msg {
            crate::bch_info!(fs, "{} {}", msg.to_str().unwrap_or("(progress)"), s);
        }
    }

    /// What BCH_IOCTL_RECOVERY_STATUS reports: units, seen, total - from the
    /// one copy.
    pub fn status(&self) -> (c::bch_progress_units, u64, u64) {
        let s = self.state.read();
        (s.units, s.seen, s.total)
    }
}

/// How far the work has got: as bch2_progress_to_text().
impl ToText for c::progress_indicator {
    fn to_text(&self, out: &mut Printbuf) {
        self.state.read().to_text(out)
    }
}

/// The filesystem's recovery pass progress indicator, c->recovery.progress -
/// one for the whole filesystem, as passes run one at a time.
pub struct Progress<'f> {
    p: &'f c::progress_indicator,
}

impl<'f> Progress<'f> {
    /// c->recovery.progress, init()ed.
    pub fn recovery(
        fs:           &'f Fs,
        msg:          &'static CStr,
        leaf_btrees:  &[c::btree_id],
        inner_btrees: &[c::btree_id],
    ) -> Self {
        let p = &fs.recovery.progress;

        p.init(fs, msg, leaf_btrees, inner_btrees);
        Progress { p }
    }

    pub fn recovery_masks(fs: &'f Fs, msg: &'static CStr, leaf_mask: u64, inner_mask: u64) -> Self {
        let p = &fs.recovery.progress;

        p.init_masks(fs, Some(msg), leaf_mask, inner_mask);
        Progress { p }
    }

    /// Report the walk reaching @iter's position.
    pub fn update<'a, 't>(&self, t: &TransAttempt<'a, 't>, iter: &BtreeIter<'t>) -> Result<(), BchError> {
        self.p.update(t, iter)
    }

    pub fn as_raw(&self) -> *mut c::progress_indicator {
        core::ptr::from_ref(self.p).cast_mut()
    }
}

// For C: its API as it was.

/// C's message: __func__, a literal, or NULL.
///
/// # Safety
/// @msg is NULL or a string that lives for good.
unsafe fn c_msg(msg: *const c_char) -> Option<&'static CStr> {
    (!msg.is_null()).then(|| unsafe { CStr::from_ptr(msg.cast()) })
}

/// bch2_progress_init().
///
/// # Safety
/// @msg is NULL or a string that lives for good: C passes __func__.
#[c_export]
pub unsafe extern "C" fn bch2_progress_init(
    s:     &c::progress_indicator,
    msg:   *const c_char,
    c:     &Opaque<c::bch_fs>,
    leaf_btree_id_mask:  u64,
    inner_btree_id_mask: u64,
) {
    s.init_masks(Fs::from_c(c), unsafe { c_msg(msg) }, leaf_btree_id_mask, inner_btree_id_mask)
}

/// bch2_progress_init_count(): for work that isn't a btree walk - no node to
/// count and no position to report, so the caller supplies the total and
/// counts its own units. Journal replay goes through a sorted array of keys.
///
/// # Safety
/// @msg is NULL or a string that lives for good.
#[c_export]
pub unsafe extern "C" fn bch2_progress_init_count(
    s:     &c::progress_indicator,
    msg:   *const c_char,
    units: c::bch_progress_units,
    total: u64,
) {
    s.init_count(unsafe { c_msg(msg) }, units, total)
}

/// bch2_progress_update_iter().
#[c_export]
pub fn bch2_progress_update_iter<'t>(
    trans: &'t BtreeTrans<'t>,
    s:     &c::progress_indicator,
    iter:  &mut BtreeIter<'t>,
) -> core::ffi::c_int {
    ret_to_c(s.update(&unsafe { trans.attempt_in_progress() }, iter))
}

/// bch2_progress_update_count().
#[c_export]
pub fn bch2_progress_update_count(c: &Opaque<c::bch_fs>, s: &c::progress_indicator) {
    s.update_count(Fs::from_c(c))
}

/// bch2_progress_to_text().
#[c_export]
#[cold]
pub fn bch2_progress_to_text(out: &mut c::printbuf, s: &c::progress_indicator) {
    s.to_text(unsafe { Printbuf::borrow_raw(out) });
}

/// What BCH_IOCTL_RECOVERY_STATUS copies out, from one consistent read.
#[c_export]
pub fn bch2_progress_status(s: &c::progress_indicator, units: &mut u32, seen: &mut u64,
                            total: &mut u64) {
    let (u, n, t) = s.status();
    (*units, *seen, *total) = (u.0, n, t);
}

/// Whether it has a total - an estimate of its work's size - to show.
#[c_export]
pub fn bch2_progress_has_total(s: &c::progress_indicator) -> bool {
    s.status().2 != 0
}

