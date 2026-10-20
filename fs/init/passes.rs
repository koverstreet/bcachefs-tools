// SPDX-License-Identifier: GPL-2.0

//! Recovery passes (init/passes.h). The pass table and the code that runs
//! it are C; a pass converted to Rust is a function the table calls by name.
//!
//! What the superblock records of passes is here: their stable ids, and the
//! recovery_passes section.

/// Export @pass, a function or closure of `&Fs` returning `Result<(), E>`
/// with E converting to BchError, as the recovery pass the C pass table calls
/// @name - its prototype C's from the header of the file this is in, as
/// #[c_export]'s:
///
/// `recovery_pass!(bch2_check_dirents => check_dirents);`
/// `recovery_pass!(bch2_resume_logged_ops_early => |fs| resume_logged_ops(fs, true));`
#[macro_export]
macro_rules! recovery_pass {
    ($name:ident => $pass:expr) => {
        /// # Safety
        /// @c is a live filesystem: the pass table's caller.
        #[::cstruct_macros::c_export]
        pub unsafe extern "C" fn $name(c: *mut $crate::c::bch_fs) -> core::ffi::c_int {
            let fs = unsafe { $crate::fs::Fs::borrow_raw(c) };

            $crate::errcode::ret_to_c(($pass)(fs))
        }
    };
}

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::locking::{Guard, Lock};
use crate::sb::io::{sb_field_ops, SbFieldOps};
use crate::types::{Le32, Le64};
use crate::util::bitmap::LeBitmap;
use crate::util::ffi::Opaque;
use crate::util::kernel::{ktime_get_real_seconds, NSEC_PER_SEC};
use crate::util::vstructs::{Head, Validated};
use crate::util::Printbuf;
use core::ffi::c_uint;
use core::mem::size_of;
use cstruct_macros::c_export;

/// Schedule recovery pass @pass, saying so to @out: as
/// bch2_run_explicit_recovery_pass(). An error - a restart of recovery - only
/// if it has to go back to an earlier pass.
pub fn run_explicit(
    fs:    &Fs,
    out:   &mut Printbuf,
    pass:  c::bch_recovery_pass,
    flags: c::bch_run_recovery_pass_flags,
) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_run_explicit_recovery_pass(fs.as_ptr(), out.as_raw(), pass, flags) })
}

/// Have recovery pass @pass run before going on, if it hasn't this mount: as
/// bch2_require_recovery_pass().
pub fn require(fs: &Fs, out: &mut Printbuf, pass: c::bch_recovery_pass) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_require_recovery_pass(fs.as_ptr(), out.as_raw(), pass) })
}

/// Whether btree @btree has been checked consistent and not changed since: as
/// bch2_btree_is_clean().
pub fn btree_is_clean(fs: &Fs, btree: c::btree_id) -> bool {
    let clean = unsafe { (*fs.as_ptr()).sb.btrees_clean };
    clean & (1u64 << btree.0) != 0
}

/// Record btree @btree as checked consistent: as bch2_set_btree_clean().
pub fn set_btree_clean(fs: &Fs, btree: c::btree_id) {
    unsafe { c::bch2_set_btree_clean(fs.as_ptr(), btree) }
}

/// Whether recovery has @pass still to run, or running: as
/// recovery_pass_will_run(). Never true outside recovery.
pub fn will_run(fs: &Fs, pass: c::bch_recovery_pass) -> bool {
    fs.flag(c::bch_fs_flags::BCH_FS_in_recovery) &&
        unsafe { (*fs.as_ptr()).recovery.current_passes } & (1u64 << pass as u32) != 0
}

/// Btree @btree is being changed, so no longer known consistent: as
/// bch2_clear_btree_clean().
pub fn clear_btree_clean(fs: &Fs, btree: c::btree_id) {
    unsafe { c::bch2_clear_btree_clean(fs.as_ptr(), btree) }
}

/// Whether recovery work should stop - the filesystem going read-only, or
/// the kthread running it told to: as bch2_recovery_cancelled().
pub fn cancelled(fs: &Fs) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::rust_bch2_recovery_cancelled(fs.as_ptr()) })
}

/// lockdep_assert_held(&c->recovery.run_lock): passes run under it - and
/// what runs as a pass, it serializes.
pub fn assert_run_lock_held(fs: &Fs) {
    unsafe { c::rust_bch2_recovery_run_lock_assert_held(fs.as_ptr()) }
}

/// recovery.run_lock, held: no recovery pass runs - to run one's work outside
/// the pass framework.
pub fn run_lock(fs: &Fs) -> Guard<'_, c::mutex> {
    Lock::from_c(&fs.recovery.run_lock).lock()
}

/// Each pass's state: bch2_recovery_pass_status_to_text().
pub fn status_to_text(fs: &Fs, out: &mut Printbuf) {
    unsafe { c::bch2_recovery_pass_status_to_text(out.as_raw(), fs.as_ptr()) }
}

// ── Stable ids ───────────────────────────────────────────────────────────────
//
// Passes are numbered in the order they run, which changes as passes are
// added and reordered; the superblock records them by stable id, which never
// changes. An id no pass has now - a pass since removed - is pass 0,
// recovery_pass_empty, as in C.

const NR: usize = c::bch_recovery_pass::BCH_RECOVERY_PASS_NR as usize;

macro_rules! pass_table {
    ($(($name:ident, $stable:literal, $($rest:tt)*)),* $(,)?) => {
        /// By pass.
        const NAMES: [&str; NR] = [$(stringify!($name)),*];
        /// Each pass's stable id, by pass.
        const TO_STABLE: [u8; NR] = [$($stable),*];
    };
}
c::BCH_RECOVERY_PASSES!(pass_table);

const STABLE_NR: usize = {
    let (mut max, mut i) = (0, 0);
    while i < NR {
        if TO_STABLE[i] > max {
            max = TO_STABLE[i];
        }
        i += 1;
    }
    max as usize + 1
};

/// Each stable id's pass, by id. Evaluated at build time, so an id two passes
/// have fails the build.
const FROM_STABLE: [u8; STABLE_NR] = {
    let (mut map, mut seen, mut i) = ([0; STABLE_NR], [false; STABLE_NR], 0);
    while i < NR {
        let id = TO_STABLE[i] as usize;
        assert!(!seen[id], "recovery pass stable id reused");
        (map[id], seen[id]) = (i as u8, true);
        i += 1;
    }
    map
};

// a mask of either is a u64
const _: () = assert!(NR <= 64 && STABLE_NR <= 64);

impl c::bch_recovery_pass {
    pub fn to_stable(self) -> c::bch_recovery_pass_stable {
        c::bch_recovery_pass_stable(TO_STABLE[self as usize].into())
    }

    pub fn name(self) -> &'static str {
        NAMES[self as usize]
    }
}

/// Stable id @stable's pass, as an index.
fn from_stable(stable: usize) -> usize {
    FROM_STABLE.get(stable).map_or(0, |&p| p.into())
}

/// Mask @v, each bit @i moved to @map[i].
fn map_bits(v: u64, map: &[u8]) -> u64 {
    map.iter().enumerate().filter(|&(i, _)| v & 1 << i != 0).fold(0, |r, (_, &to)| r | 1 << to)
}

/// A mask of passes as their stable ids.
pub fn passes_to_stable(v: u64) -> u64 {
    map_bits(v, &TO_STABLE)
}

/// A mask of stable ids as the passes they are.
pub fn passes_from_stable(v: u64) -> u64 {
    map_bits(v, &FROM_STABLE)
}

#[c_export]
pub fn bch2_recovery_pass_to_stable(pass: c::bch_recovery_pass) -> c::bch_recovery_pass_stable {
    pass.to_stable()
}

#[c_export]
pub fn bch2_recovery_passes_to_stable(v: u64) -> u64 {
    passes_to_stable(v)
}

#[c_export]
pub fn bch2_recovery_passes_from_stable(v: u64) -> u64 {
    passes_from_stable(v)
}

/// The recovery passes in @stable - bits of their stable index, as the
/// superblock stores them - by name.
pub fn prt_stable(out: &mut Printbuf, stable: u64) {
    let passes = passes_from_stable(stable);
    let names = NAMES.iter().enumerate().filter(|&(p, _)| passes & 1 << p != 0);
    for (i, (_, name)) in names.enumerate() {
        if i != 0 {
            write!(out, ",");
        }
        write!(out, "{name}");
    }
}

// ── The superblock section ───────────────────────────────────────────────────

/// When each pass last ran and how long it took, by stable id: what
/// ratelimiting a pass goes by.
impl SbFieldOps for c::bch_sb_field_recovery_passes {
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, r: &Self) {
        while out.nr_tabstops() < 2 {
            out.tabstop_push(32);
        }

        write!(out, "Pass\tLast run\tLast runtime\n");
        for (stable, e) in r.start.iter().enumerate().filter(|(_, e)| e.last_run.get() != 0) {
            write!(out, "{}\t", NAMES[from_stable(stable)]);
            out.datetime(e.last_run.get() as i64);
            out.tab();
            out.time_units(u64::from(e.last_runtime.get()) * NSEC_PER_SEC);
            if e.bch_recovery_pass_no_ratelimit() {
                write!(out, " (no ratelimit)");
            }
            out.newline();
        }
    }
}

#[c_export]
pub static bch_sb_field_ops_recovery_passes: c::bch_sb_field_ops =
    sb_field_ops::<c::bch_sb_field_recovery_passes>();

// ── Its entries ──────────────────────────────────────────────────────────────

type Section = c::bch_sb_field_recovery_passes;

/// @pass's entry, if the section has one.
fn entry(sb: &c::bch_sb, pass: c::bch_recovery_pass) -> Option<c::recovery_pass_entry> {
    sb.field::<Section>()?.start.get(pass.to_stable().0 as usize).copied()
}

/// @pass's entry, the section made or grown to hold it: None if there's no
/// room in the superblock.
fn entry_mut<'a>(fs: &Fs, sb: &'a mut SbLockGuard, pass: c::bch_recovery_pass)
    -> Option<&'a mut c::recovery_pass_entry>
{
    let stable = pass.to_stable().0 as usize;
    let bytes = size_of::<Head<Section>>() + (stable + 1) * size_of::<c::recovery_pass_entry>();
    let Some(r) = sb.field_get_minsize::<Section>((bytes / size_of::<u64>()) as u32) else {
        crate::bch_err!(fs, "error creating recovery_passes sb section");
        return None;
    };
    Some(&mut r.start[stable])
}

/// Whether a pass that last ran as @e says should hold off: its last runtime
/// was more than 1/@runtime_fraction of the time since, and it isn't marked
/// never to be. Both ratelimits go by it: the superblock's, of expensive
/// passes, and the in-memory one of failing passes, each with its own entries.
pub fn entry_ratelimited(e: &c::recovery_pass_entry, runtime_fraction: u32) -> bool {
    let now = ktime_get_real_seconds() as u64;
    !e.bch_recovery_pass_no_ratelimit() &&
        u64::from(e.last_runtime.get()) * u64::from(runtime_fraction) > now.wrapping_sub(e.last_run.get())
}

fn want_ratelimit_locked(sb: &c::bch_sb, pass: c::bch_recovery_pass, runtime_fraction: u32) -> bool {
    entry(sb, pass).is_some_and(|e| entry_ratelimited(&e, runtime_fraction))
}

/// Whether recovery pass @pass should hold off, by when it last ran and how
/// long it took: entry_ratelimited().
pub fn want_ratelimit(fs: &Fs, pass: c::bch_recovery_pass, runtime_fraction: u32) -> bool {
    want_ratelimit_locked(fs.sb_lock().sb(), pass, runtime_fraction)
}

/// Have recovery pass @pass run when it's next wanted, however recently it
/// ran: a repair that found more to do.
pub fn set_no_ratelimit(fs: &Fs, pass: c::bch_recovery_pass) {
    let mut sb = fs.sb_lock();
    if let Some(e) = entry_mut(fs, &mut sb, pass).filter(|e| !e.bch_recovery_pass_no_ratelimit()) {
        e.set_bch_recovery_pass_no_ratelimit(true);
        fs.write_super(&mut sb);
    }
}

/// Recovery pass @pass, started at @start_time, has completed: it's no
/// longer required, and its entry says when it ran and for how long. If it
/// was the last of this run's passes, the errors to fix silently - which came
/// with the passes required - are done with too.
fn pass_complete(fs: &Fs, pass: c::bch_recovery_pass, start_time: i64) {
    let mut sb = fs.sb_lock();
    let ext = sb.field_mut::<c::bch_sb_field_ext<Validated>>().expect("an ext section");
    LeBitmap::from_words_mut(&mut ext.recovery_passes_required).clear(pass.to_stable().0 as usize);
    // SAFETY: read without recovery.lock, as C does; the runner, calling,
    // took this pass off it before running it
    if unsafe { (*fs.as_ptr()).recovery.current_passes } == 0 {
        ext.errors_silent.fill(Le64::new(0));
    }

    if let Some(e) = entry_mut(fs, &mut sb, pass) {
        let end_time = ktime_get_real_seconds();
        e.last_run = Le64::new(end_time as u64);
        e.last_runtime = Le32::new((end_time - start_time).max(0) as u32);
        e.set_bch_recovery_pass_no_ratelimit(false);
    }
    fs.write_super(&mut sb);
}

#[c_export]
pub fn bch2_sb_recovery_pass_complete(c: &Opaque<c::bch_fs>, pass: c::bch_recovery_pass, start_time: i64) {
    pass_complete(Fs::from_c(c), pass, start_time)
}

#[c_export]
pub fn bch2_recovery_pass_want_ratelimit_locked(c: &Opaque<c::bch_fs>, pass: c::bch_recovery_pass,
                                                runtime_fraction: c_uint) -> bool {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    want_ratelimit_locked(unsafe { fs.sb_lock_held() }.sb(), pass, runtime_fraction)
}

#[c_export]
pub fn bch2_recovery_pass_entry_ratelimited(e: &c::recovery_pass_entry, runtime_fraction: c_uint) -> bool {
    entry_ratelimited(e, runtime_fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pass is its stable id's; an id no pass has is pass 0; and a mask
    /// goes there and back.
    #[test]
    fn stable_ids() {
        for p in 0..NR {
            assert_eq!(from_stable(TO_STABLE[p].into()), p);
        }
        let unused = (0..STABLE_NR).find(|&id| !TO_STABLE.contains(&(id as u8)));
        if let Some(id) = unused {
            assert_eq!(from_stable(id), 0);
        }
        assert_eq!(from_stable(STABLE_NR), 0);

        let all = u64::MAX >> (64 - NR);
        assert_eq!(passes_from_stable(passes_to_stable(all)), all);

        let check_topology = c::bch_recovery_pass::BCH_RECOVERY_PASS_check_topology;
        assert_eq!((check_topology.to_stable().0, check_topology.name()), (4, "check_topology"));
    }
}
