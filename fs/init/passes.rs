// SPDX-License-Identifier: GPL-2.0

//! Recovery passes (init/passes.h): the table of them, what the superblock
//! records of them, scheduling one - from the pass that finds it's needed,
//! repair found anywhere - and running them, at mount and in the background.
//!
//! A pass is C's or Rust's: the table calls each by name, bch2_<pass>(), and
//! a Rust pass exports itself under it with recovery_pass!().
//!
//! Passes are numbered in the order they run; the superblock records them by
//! stable id, which never changes: in ext's recovery_passes_required, the
//! passes it needs run, and in the recovery_passes section, when each last ran
//! and for how long - which ratelimits expensive repairs.
//!
//! The runner's state is bch_fs_recovery's, its runs serialized by run_lock:
//! recovery.runner, a spinlock - taken with interrupts off, as scheduling can
//! come from where they are - over what only the runner reads, publishing
//! what others read unlocked, a word at a time.

/// Export @pass, a function or closure of `&Fs` returning `Result<(), E>`
/// with E converting to BchError, as the recovery pass the pass table calls
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

use crate::btree::iter::lockrestart_do;
use crate::c;
use crate::c::{bch_recovery_runner as Runner, RecoveryWords};
use crate::errcode::{bch_errcode, ret_to_c, ret_to_result_void, BchError};
use crate::fs::{Fs, FsWork, SbLockGuard, WriteRef};
use crate::locking::Guard;
use crate::sb::io::{sb_field_ops, SbFieldOps, SbWrite};
use crate::snapshots::subvolume;
use crate::types::{Le32, Le64};
use crate::util::bitmap::LeBitmap;
use crate::util::ffi::{c_char, Opaque};
use crate::util::kernel::{ktime_get_ns, ktime_get_real_seconds, NSEC_PER_SEC};
use crate::util::kthread::Kthread;
use crate::util::log::{log_cont, log_start, LogLevel, LogMsg};
use crate::util::projections::AtomicWord;
use crate::util::vstructs::{Head, Validated};
use crate::util::workqueue::Queue;
use crate::util::Printbuf;
use crate::{bch_err, bch_info, c_function_name, inode, message, prt};
use core::ffi::{c_int, c_uint, c_ulong};
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, Ordering};
use cstruct_macros::c_export;

type Pass = c::bch_recovery_pass;
type Flags = c::bch_run_recovery_pass_flags;

// ── The passes ───────────────────────────────────────────────────────────────

const NR: usize = Pass::BCH_RECOVERY_PASS_NR as usize;

/// Each pass's function: C's, or Rust's under its C name. A module of their
/// own, as this one defines two of them.
mod pass_fns {
    macro_rules! pass_fns {
        ($(($name:ident, $($rest:tt)*)),* $(,)?) => { ::paste::paste! {
            unsafe extern "C" {
                $(pub fn [<bch2_ $name>](c: *mut crate::c::bch_fs) -> core::ffi::c_int;)*
            }

            /// By pass.
            pub const RUN: [unsafe extern "C" fn(*mut crate::c::bch_fs) -> core::ffi::c_int; super::NR] =
                [$([<bch2_ $name>]),*];
        } };
    }
    crate::c::BCH_RECOVERY_PASSES!(pass_fns);
}
use pass_fns::RUN;

macro_rules! pass_table {
    ($(($name:ident, $stable:literal, $when:expr, $depends:expr, $($desc:literal)*)),* $(,)?) => {
        /// By pass.
        const NAMES: [&str; NR] = [$(stringify!($name)),*];

        /// The names, NULL terminated, for C's option parsing and printing.
        #[c_export]
        #[c("const char * const bch2_recovery_passes[]")]
        pub static bch2_recovery_passes: [*const c_char; NR + 1] =
            [$(concat!(stringify!($name), "\0").as_ptr().cast(),)* core::ptr::null()];

        /// Each pass's stable id, by pass.
        const TO_STABLE: [u8; NR] = [$($stable),*];
        /// When each pass runs - PASS_* - by pass.
        const WHEN: [c_ulong; NR] = {
            use c::*;
            [$($when),*]
        };
        /// The passes each depends on, by pass: run first, if both are to be.
        const DEPENDS: [u64; NR] = {
            use c::*;
            #[allow(non_snake_case)]
            const fn BIT_ULL(nr: u32) -> u64 {
                1 << nr
            }
            [$($depends),*]
        };
    };
}
c::BCH_RECOVERY_PASSES!(pass_table);

// a mask of passes is a u64
const _: () = assert!(NR <= 64);

/// Pass 0, recovery_pass_empty: what current_pass, rewound_to and
/// rewound_from hold for none.
const NONE: Pass = Pass::BCH_RECOVERY_PASS_recovery_pass_empty;

fn bit(pass: Pass) -> u64 {
    1 << pass as u32
}

impl c::bch_recovery_pass {
    pub fn name(self) -> &'static str {
        NAMES[self as usize]
    }

    fn when(self) -> c_ulong {
        WHEN[self as usize]
    }
}

/// Pass number @pass's name, if it's one this version has.
pub fn name(pass: u32) -> Option<&'static str> {
    NAMES.get(pass as usize).copied()
}

/// The passes that run at any of @flags, PASS_*.
pub fn passes_match(flags: c_ulong) -> u64 {
    (0..NR).filter(|&p| WHEN[p] & flags != 0).fold(0, |m, p| m | 1 << p)
}

/// The passes an fsck runs.
pub fn fsck_passes() -> u64 {
    passes_match(c::PASS_FSCK)
}

#[c_export]
pub fn bch2_recovery_passes_match(flags: c_uint) -> u64 {
    passes_match(flags.into())
}

/// A placeholder, so scan_for_btree_nodes isn't pass 0.
fn recovery_pass_empty(_fs: &Fs) -> Result<(), BchError> {
    Ok(())
}
crate::recovery_pass!(bch2_recovery_pass_empty => recovery_pass_empty);

/// That the root inode can be read, while recovery can still rewind to
/// repair it.
fn lookup_root_inode(fs: &Fs) -> Result<(), BchError> {
    let inum = c::subvol_inum { subvol: c::BCACHEFS_ROOT_SUBVOL.into(), inum: c::BCACHEFS_ROOT_INO.into() };
    let trans = crate::btree_trans!(fs);
    lockrestart_do(&trans, |t| {
        subvolume::get(t, c::BCACHEFS_ROOT_SUBVOL, true)?;
        inode::find_by_inum_trans(t, inum, c_function_name!()).map(drop)
    })
}
crate::recovery_pass!(bch2_lookup_root_inode => lookup_root_inode);

// ── Stable ids ───────────────────────────────────────────────────────────────
//
// What the superblock records passes by: a pass's number changes as passes
// are added and reordered, its stable id never does. An id no pass has now -
// a pass since removed - is pass 0, recovery_pass_empty, as in C.

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

// a mask of stable ids is a u64
const _: () = assert!(STABLE_NR <= 64);

impl c::bch_recovery_pass {
    pub fn to_stable(self) -> c::bch_recovery_pass_stable {
        c::bch_recovery_pass_stable(TO_STABLE[self as usize].into())
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
    out.bitflags(&NAMES, passes_from_stable(stable))
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

/// For self healing - e.g. missing backpointers - that would otherwise keep
/// scheduling an expensive repair pass that can wait: run it at most once per
/// this many times its last runtime. Not the failing-pass backoff, which is
/// FAILING_RATELIMIT.
const SELF_HEAL_RATELIMIT: u32 = 100;

/// @pass's entry, if the section has one.
fn entry(sb: &c::bch_sb, pass: Pass) -> Option<c::recovery_pass_entry> {
    sb.field::<Section>()?.start.get(pass.to_stable().0 as usize).copied()
}

/// @pass's entry, the section made or grown to hold it: None if there's no
/// room in the superblock.
fn entry_mut<'a>(fs: &Fs, sb: &'a mut SbLockGuard, pass: Pass) -> Option<&'a mut c::recovery_pass_entry> {
    let stable = pass.to_stable().0 as usize;
    let bytes = size_of::<Head<Section>>() + (stable + 1) * size_of::<c::recovery_pass_entry>();
    let Some(r) = sb.field_get_minsize::<Section>((bytes / size_of::<u64>()) as u32) else {
        bch_err!(fs, "error creating recovery_passes sb section");
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

fn want_ratelimit_locked(sb: &c::bch_sb, pass: Pass, runtime_fraction: u32) -> bool {
    entry(sb, pass).is_some_and(|e| entry_ratelimited(&e, runtime_fraction))
}

/// Whether recovery pass @pass should hold off, by when it last ran and how
/// long it took: entry_ratelimited().
pub fn want_ratelimit(fs: &Fs, pass: Pass, runtime_fraction: u32) -> bool {
    want_ratelimit_locked(fs.sb_lock().sb(), pass, runtime_fraction)
}

/// Have recovery pass @pass run when it's next wanted, however recently it
/// ran: a repair that found more to do.
pub fn set_no_ratelimit(fs: &Fs, pass: Pass) {
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
fn pass_complete(fs: &Fs, pass: Pass, start_time: i64) {
    let mut sb = fs.sb_lock();
    let ext = sb.field_mut::<c::bch_sb_field_ext<Validated>>().expect("an ext section");
    LeBitmap::from_words_mut(&mut ext.recovery_passes_required).clear(pass.to_stable().0 as usize);
    // the runner takes each pass off current_passes before running it: none
    // left, this was the last
    if fs.recovery.runner.current_passes.get() == 0 {
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

// ── Btrees checked clean ─────────────────────────────────────────────────────

/// Whether btree @btree has been checked consistent and not changed since: as
/// bch2_btree_is_clean().
pub fn btree_is_clean(fs: &Fs, btree: c::btree_id) -> bool {
    fs.sb.btrees_clean & (1u64 << btree.0) != 0
}

/// Record btree @btree as checked consistent: as bch2_set_btree_clean().
pub fn set_btree_clean(fs: &Fs, btree: c::btree_id) {
    unsafe { c::bch2_set_btree_clean(fs.as_ptr(), btree) }
}

/// Btree @btree is being changed, so no longer known consistent: as
/// bch2_clear_btree_clean().
pub fn clear_btree_clean(fs: &Fs, btree: c::btree_id) {
    unsafe { c::bch2_clear_btree_clean(fs.as_ptr(), btree) }
}

// ── The runner's state ───────────────────────────────────────────────────────

/// The runner's lock, held: its state, and permission to write what of
/// recovery's is published. Nothing sleeps while it's held.
type Recovery<'a> = Guard<'a, c::spinlock_t, Runner, RecoveryWords>;

fn lock(fs: &Fs) -> Recovery<'_> {
    fs.recovery.runner.lock()
}

// SAFETY: C's enum, a u32; zeroes are pass 0, NONE - and it's only ever set()
// to a Pass
unsafe impl AtomicWord for Pass {
    type Atomic = AtomicU32;

    fn load(a: &AtomicU32) -> Pass {
        Pass::from_bits(a.load(Ordering::Relaxed))
    }

    fn store(a: &AtomicU32, pass: Pass) {
        a.store(pass as u32, Ordering::Relaxed)
    }
}

/// Whether recovery has @pass still to run, or running: as
/// recovery_pass_will_run(). Never true outside recovery.
pub fn will_run(fs: &Fs, pass: Pass) -> bool {
    fs.flag(c::bch_fs_flags::BCH_FS_in_recovery) && fs.recovery.runner.current_passes.get() & bit(pass) != 0
}

/// Whether recovery work should stop - the filesystem going read-only, or
/// the kthread running it told to.
pub fn cancelled(fs: &Fs) -> Result<(), BchError> {
    if fs.flag(c::bch_fs_flags::BCH_FS_going_ro) {
        return fs.throw(bch_errcode::BCH_ERR_erofs_recovery_cancelled);
    }
    if Kthread::should_stop() {
        return fs.throw(bch_errcode::BCH_ERR_recovery_cancelled);
    }
    Ok(())
}

/// recovery.run_lock is held: passes run under it - and what runs as a pass,
/// it serializes.
pub fn assert_run_lock_held(fs: &Fs) {
    fs.recovery.run_lock.assert_held()
}

/// recovery.run_lock, held: no recovery pass runs - to run one's work outside
/// the pass framework.
pub fn run_lock(fs: &Fs) -> Guard<'_, c::mutex> {
    fs.recovery.run_lock.lock()
}

/// Recovery done through @pass, without the runner: a new filesystem's,
/// which has nothing to recover.
#[c_export]
pub fn bch2_recovery_set_pass_done(c: &Opaque<c::bch_fs>, pass: c::bch_recovery_pass) {
    let fs = Fs::from_c(c);
    lock(fs).published().pass_done.set(pass);
}

/// The async runner's work item, for bch_fs_recovery's init.
pub fn async_work_init() -> impl ::pin_init::PinInit<FsWork> {
    #[cfg(kernel)]
    return FsWork::new(async_work, c"&c->recovery.work", ::kernel::static_lock_class!());
    #[cfg(not(kernel))]
    return FsWork::new(async_work);
}

// ── Scheduling ───────────────────────────────────────────────────────────────

/// Every pass that depends on @pass, transitively, and @pass.
fn pass_dependents(pass: usize) -> u64 {
    let mut passes = 1u64 << pass;
    loop {
        let more = (0..NR).filter(|&i| passes & 1 << i == 0 && passes & DEPENDS[i] != 0)
                          .fold(0, |m, i| m | 1 << i);
        if more == 0 {
            return passes;
        }
        passes |= more;
    }
}

/// Whether @pass, and those of @passes that depend on it, can all run online -
/// in the background, after mount.
fn should_defer(pass: usize, passes: u64) -> bool {
    let passes = (passes & pass_dependents(pass)) | 1 << pass;
    passes == passes & passes_match(c::PASS_ONLINE)
}

/// Whether this run may execute @pass, as opposed to only scheduling it: the
/// mount path runs before BCH_FS_started and may run anything, the async
/// runner and the online-fsck ioctl run after it against a live filesystem.
/// Both bound their starting set, but a pass required mid-run goes straight
/// into current_passes - which is how an offline pass reaches a live
/// filesystem.
fn may_run_now(fs: &Fs, pass: Pass) -> bool {
    !fs.flag(c::bch_fs_flags::BCH_FS_started) || pass.when() & c::PASS_ONLINE != 0
}

/// Whether running @pass means going back for it: the pass loop is running,
/// is past it, and hasn't run it this run.
fn needs_rewind(fs: &Fs, r: &Recovery<'_>, pass: Pass) -> bool {
    fs.flag(c::bch_fs_flags::BCH_FS_running_recovery_passes) &&
        fs.recovery.runner.current_pass.get() > pass &&
        r.passes_attempted & bit(pass) == 0
}

/// Never record scan_for_btree_nodes in the superblock: check_topology runs it
/// if required, so the requirement is always re-derivable.
fn is_persistent(pass: Pass) -> bool {
    pass != Pass::BCH_RECOVERY_PASS_scan_for_btree_nodes
}

/// Whether scheduling @pass would do anything. With RUN_RECOVERY_PASS_ratelimit
/// in @flags, the superblock - @sb, under sb_lock - decides whether it's to be
/// ratelimited, and if not it's taken out of @flags.
fn needs_set(fs: &Fs, r: &Recovery<'_>, pass: Pass, flags: &mut Flags, sb: Option<&c::bch_sb>) -> bool {
    if flags.contains(Flags::RUN_RECOVERY_PASS_skip_if_complete) &&
       fs.recovery.runner.passes_complete.get() & bit(pass) != 0 {
        return false;
    }

    if flags.contains(Flags::RUN_RECOVERY_PASS_ratelimit) &&
       !want_ratelimit_locked(sb.expect("sb_lock, to ratelimit"), pass, SELF_HEAL_RATELIMIT) {
        flags.remove(Flags::RUN_RECOVERY_PASS_ratelimit);
    }

    /*
     * For a non-persistent pass we don't want to do anything if the pass has
     * already run: it means we need a prior pass to run before we continue
     * to repair, and we don't expect that pass to fix the damage we
     * encountered.
     *
     * Otherwise, we run run_explicit_recovery_pass when we find damage, so
     * it should run again even if it's already run.
     *
     * XXX: the "!in_recovery ||" here has no counterpart in
     * run_explicit_sb(), which routes to the ephemeral set on
     * !is_persistent() alone. So an online request checks the superblock set
     * while recording into the ephemeral one. Harmless today only because
     * scan_for_btree_nodes can never reach the superblock set - so this arm
     * always says "not already requested".
     */
    let in_recovery = fs.flag(c::bch_fs_flags::BCH_FS_in_recovery);
    let already_running = if !in_recovery || is_persistent(pass) {
        fs.sb.recovery_passes_required
    } else {
        fs.recovery.runner.current_passes.get()
    };

    if already_running & bit(pass) == 0 {
        return true;
    }

    if !flags.contains(Flags::RUN_RECOVERY_PASS_ratelimit) && r.passes_ratelimiting & bit(pass) != 0 {
        return true;
    }

    needs_rewind(fs, r, pass)
}

/// Schedule recovery pass @pass, saying so to @out - or, if recovery is past
/// it, rewind to it: restart_recovery. @w is permission to persist, in the
/// superblock - the caller's, under sb_lock - and None for an ephemeral
/// schedule, which may come from a btree transaction commit, that can take
/// neither: only in-memory scheduling is reachable then.
fn run_explicit_sb(fs: &Fs, out: &mut Printbuf, pass: Pass, mut flags: Flags, w: Option<&mut SbWrite<'_>>)
    -> Result<(), BchError>
{
    debug_assert_eq!(w.is_none(), flags.contains(Flags::RUN_RECOVERY_PASS_ephemeral));

    let opts = fs.opts();
    if opts.norecovery != 0 || opts.recovery_passes_exclude & bit(pass) != 0 {
        return Ok(());
    }

    out.make_room_gfp(1024, c::GFP_KERNEL);
    let mut out = out.atomic();
    let mut r = lock(fs);
    let rec = &fs.recovery.runner;

    if !needs_set(fs, &r, pass, &mut flags, w.as_ref().map(|w| w.sb())) {
        return Ok(());
    }

    out.set_suppressed(false);

    let running = fs.flag(c::bch_fs_flags::BCH_FS_running_recovery_passes);
    let ratelimit = flags.contains(Flags::RUN_RECOVERY_PASS_ratelimit);

    if flags.contains(Flags::RUN_RECOVERY_PASS_ephemeral) || !is_persistent(pass) {
        *r.published().scheduled_passes_ephemeral.place() |= bit(pass);
    } else {
        let w = w.expect("not ephemeral: an sb_write");
        let ext = w.field_mut::<c::bch_sb_field_ext<Validated>>().expect("an ext section");
        let required = LeBitmap::from_words_mut(&mut ext.recovery_passes_required);
        let new = !required.test_and_set(pass.to_stable().0 as usize);
        w.record(new);
    }

    /*
     * Recorded above, but not run: repair found mid-mount is still repair,
     * and a tool that opened this filesystem to inspect it doesn't want a
     * pass starting underneath it - accounting underflow scheduling
     * check_allocations is the one that bites. The requirement stays in the
     * superblock, so the next mount that isn't skipping does the work.
     */
    if opts.recovery_passes_skip_scheduled != 0 {
        return Ok(());
    }

    if pass < Pass::BCH_RECOVERY_PASS_set_may_go_rw &&
       fs.flag(c::bch_fs_flags::BCH_FS_may_go_rw) {
        write!(out, "need recovery pass {} ({}), but already rw\n", pass.name(), pass as u32);
        return fs.throw(bch_errcode::BCH_ERR_cannot_rewind_recovery);
    }

    if ratelimit {
        r.passes_ratelimiting |= bit(pass);
    } else {
        r.passes_ratelimiting &= !bit(pass);
    }

    let rewind = needs_rewind(fs, &r, pass);

    /*
     * A pass that already ran this run - completed OR failed - must never
     * be re-run in the same loop: that's the rewind invariant, which
     * needs_rewind() enforces by gating on passes_attempted. (Gating on
     * passes_complete looped: a failing pass never completes, so under
     * errors=continue a later pass re-requesting it rewound forever.) The
     * "run it now because it can't be deferred to the background" arm has to
     * honor the same invariant: otherwise a pass that reschedules an earlier,
     * already-run pass (check_key_has_snapshot scheduling
     * check_inodes/check_extents while running check_xattrs, once the
     * dead-snapshot keys those passes would clean are still present) injects
     * it back into current_passes and we re-run the whole content-check range
     * out of order, looping.
     *
     * A pass this run may not execute falls through to the schedule branch
     * rather than erroring: the requirement still reaches the superblock, so
     * the next mount does the work. Erroring would turn a repair request
     * made from the IO path into a hard error.
     */
    let run_now = may_run_now(fs, pass) &&
        (rewind ||
         (!should_defer(pass as usize, rec.current_passes.get()) && r.passes_attempted & bit(pass) == 0));

    /*
     * Ephemeral scheduling is best-effort and must never rewind: the caller
     * may be a BTREE_TRIGGER_atomic trigger committed to its commit, which
     * can't restart recovery (and ignores our return). If the pass already
     * ran, the scheduled_passes_ephemeral backstop reruns it via the async
     * runner instead.
     */
    if running && !ratelimit && run_now && !(rewind && flags.contains(Flags::RUN_RECOVERY_PASS_ephemeral)) {
        write!(out, "running recovery pass {} ({}), currently at {} ({}){}\n",
               pass.name(), pass as u32, rec.current_pass.get().name(), rec.current_pass.get() as u32,
               if rewind { " - rewinding" } else { "" });

        *r.published().current_passes.place() |= bit(pass);

        if rewind {
            let rewound_to = if r.rewound_to != NONE { r.rewound_to.min(pass) } else { pass };
            let current = rec.current_pass.get();
            /*
             * Only log when the rewind target actually changes - the same
             * rewind gets re-requested on every call that finds the same
             * damage, and we don't want to spam for those. Logged unlocked:
             * a message allocates.
             */
            let changed = rewound_to != r.rewound_to;
            r.rewound_to = rewound_to;
            drop(r);
            if changed {
                bch_info!(fs, "recovery: rewinding to {} ({}), currently running {} ({})",
                          pass.name(), pass as u32, current.name(), current as u32);
            }
            return fs.throw(bch_errcode::BCH_ERR_restart_recovery);
        }
    } else {
        write!(out, "scheduling recovery pass {} ({}){}\n", pass.name(), pass as u32,
               if ratelimit { " - ratelimiting" } else { "" });

        if !ratelimit && pass.when() & c::PASS_ONLINE != 0 {
            run_async(fs);
        }
    }

    Ok(())
}

/// Schedule recovery pass @pass, saying so to @out. An error - a restart of
/// recovery - only if it has to go back to an earlier pass.
pub fn run_explicit(fs: &Fs, out: &mut Printbuf, pass: Pass, mut flags: Flags) -> Result<(), BchError> {
    // with RUN_RECOVERY_PASS_ratelimit, needs_set() needs sb_lock
    if !flags.contains(Flags::RUN_RECOVERY_PASS_ratelimit) &&
       !needs_set(fs, &lock(fs), pass, &mut flags, None) {
        return Ok(());
    }

    /*
     * An ephemeral schedule only touches in-memory recovery state under the
     * runner's lock. Its callers may hold btree locks, so sb_lock is
     * unavailable to them - and with no sb_lock there's no permission to
     * persist:
     */
    if flags.contains(Flags::RUN_RECOVERY_PASS_ephemeral) {
        return run_explicit_sb(fs, out, pass, flags, None);
    }

    run_explicit_sb(fs, out, pass, flags, Some(&mut fs.sb_write()))
}

/// Have recovery pass @pass run before going on, if it hasn't recently: Ok
/// if it has; restart_recovery if it's in recovery's past, which rewinds to
/// it; else recovery_pass_will_run - it's scheduled, and runs later.
pub fn require(fs: &Fs, out: &mut Printbuf, pass: Pass) -> Result<(), BchError> {
    if fs.flag(c::bch_fs_flags::BCH_FS_running_recovery_passes) &&
       fs.recovery.runner.passes_complete.get() & bit(pass) != 0 {
        return Ok(());
    }

    let mut w = fs.sb_write();

    if want_ratelimit_locked(w.sb(), pass, SELF_HEAL_RATELIMIT) {
        return Ok(());
    }

    /*
     * restart_recovery must only come from an actually-armed rewind, or the
     * loop sees restart_recovery with rewound_to unset and fails.
     */
    run_explicit_sb(fs, out, pass, Flags::empty(), Some(&mut w))?;
    fs.throw(bch_errcode::BCH_ERR_recovery_pass_will_run)
}

#[c_export]
pub fn __bch2_run_explicit_recovery_pass(c: &Opaque<c::bch_fs>, out: &mut Printbuf, pass: c::bch_recovery_pass,
                                         flags: c::bch_run_recovery_pass_flags,
                                         w: Option<&mut c::sb_write>) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C's - its CLASS(sb_write), under its sb_lock
    let mut w = w.map(|w| unsafe { SbWrite::from_c(fs, w) });
    ret_to_c(run_explicit_sb(fs, out, pass, flags, w.as_mut()))
}

#[c_export]
pub fn bch2_run_explicit_recovery_pass(c: &Opaque<c::bch_fs>, out: &mut Printbuf, pass: c::bch_recovery_pass,
                                       flags: c::bch_run_recovery_pass_flags) -> c_int {
    ret_to_c(run_explicit(Fs::from_c(c), out, pass, flags))
}

// ── Running ──────────────────────────────────────────────────────────────────

/// Retry backoff for a failing pass, as a multiple of its last runtime (same
/// units as want_ratelimit()'s fraction): a pass that ran for T before
/// failing isn't retried by automatic recovery for 2^n * T after its nth
/// consecutive failure, up to RATELIMIT * T. Exponential, not a fixed
/// multiple: a transient failure - journal_res_blocked under a full journal -
/// gets retried soon, while a pass that keeps failing still backs off to a
/// small fraction of the time. A fixed 100x kept a pass that failed once after
/// 766s unscheduled for 21 hours (tools#941). Separate from
/// SELF_HEAL_RATELIMIT: that one throttles repairs that succeed.
const FAILING_RATELIMIT: u32 = 20;

fn failing_pass_backoff(r: &Runner, pass: usize) -> u32 {
    match r.passes_failing_nr[pass] {
        nr @ 0..5 => (1 << nr).min(FAILING_RATELIMIT),
        _ => FAILING_RATELIMIT,
    }
}

fn run_pass(fs: &Fs, pass: Pass) -> Result<(), BchError> {
    let silent = pass.when() & c::PASS_SILENT != 0;
    if !silent {
        log_start(fs, LogLevel::INFO, message!("{}...", pass.name()));
    }

    let start_time = ktime_get_real_seconds();
    // SAFETY: a pass, of a live filesystem
    let ret = ret_to_result_void(unsafe { RUN[pass as usize](fs.as_ptr()) });

    if let Err(e) = &ret {
        if !e.matches(bch_errcode::BCH_ERR_restart_recovery) {
            let end_time = ktime_get_real_seconds();
            let runtime = (end_time - start_time).max(0);

            let mut r = lock(fs);
            let p = pass as usize;
            r.passes_failing |= bit(pass);
            r.passes_failing_nr[p] = r.passes_failing_nr[p].saturating_add(1);
            let (nr, backoff) = (r.passes_failing_nr[p], failing_pass_backoff(&r, p));
            /*
             * Ratelimit retries the same way the sb ratelimits expensive
             * passes, but in memory - a failing pass doesn't get to write
             * the superblock. flags stays 0, so this throttle applies even
             * to passes the sb marks NO_RATELIMIT: that flag is about not
             * throttling successful reruns, not about hammering failures.
             */
            r.passes_failing_ratelimit[p] = c::recovery_pass_entry {
                last_run:     Le64::new(end_time as u64),
                last_runtime: Le32::new(runtime as u32),
                ..Default::default()
            };
            drop(r);

            bch_err!(fs, "{}(): error {} - failure {}, not retried automatically for {}s",
                     pass.name(), e, nr, runtime as u64 * u64::from(backoff));
        }
        return ret;
    }

    if !silent {
        log_cont(fs, message!(" done ({} seconds)", ktime_get_real_seconds() - start_time));
    }

    let mut r = lock(fs);
    r.passes_failing = 0;
    r.passes_failing_nr[pass as usize] = 0;
    drop(r);

    if !fs.flag(c::bch_fs_flags::BCH_FS_error) {
        pass_complete(fs, pass, start_time);
    }
    Ok(())
}

/// Run the passes in @passes, in order, and any scheduled as they run; going
/// back for one in their past if a pass requires it. With @failfast, stop at
/// the first that fails; else the last error is returned, after running the
/// rest.
pub fn run_passes(fs: &Fs, mut passes: u64, failfast: bool) -> Result<(), BchError> {
    /*
     * The rewind machinery (needs_rewind(), require(), run_explicit_sb())
     * keys on this, not BCH_FS_in_recovery: we can rewind whenever the pass
     * loop is running, which includes the async runner - not only during
     * mount. BCH_FS_in_recovery is cleared before the async runner starts.
     */
    fs.set_flag(c::bch_fs_flags::BCH_FS_running_recovery_passes);

    let rec = &fs.recovery.runner;
    let mut r = lock(fs);

    if fs.sb.features & 1 << c::bch_sb_feature::BCH_FEATURE_no_alloc_info.0 != 0 {
        passes &= !passes_match(c::PASS_ALLOC);
    }

    /*
     * A failing pass is retried on a cost-model ratelimit: retry cadence
     * scales with how long the pass ran, so automatic recovery doesn't
     * hammer a pass that keeps failing - but, unlike waiting for some other
     * pass to succeed, it isn't stuck forever if nothing else makes
     * progress. (A success clears passes_failing wholesale below, so a pass
     * that fixes a dependency still triggers an immediate retry.)
     *
     * An explicit fsck is different: the user asked for these passes, so
     * run them even if they failed before, and let them fail loudly again
     * rather than silently returning success having done nothing.
     */
    if !fs.flag(c::bch_fs_flags::BCH_FS_in_fsck) {
        for p in (0..NR).filter(|&p| r.passes_failing & 1 << p != 0) {
            if entry_ratelimited(&r.passes_failing_ratelimit[p], failing_pass_backoff(&r, p)) {
                passes &= !(1 << p);
            }
        }
    }

    r.published().current_passes.set(passes);
    /*
     * Fresh attempt-epoch per run: a pass attempted in a previous (online)
     * run is fair game again; within one run, attempted passes are never
     * rewound to or re-queued behind current_pass.
     */
    r.passes_attempted = 0;

    /*
     * Passes scheduled ephemerally mid-run (e.g. delete_dead_snapshots via a
     * trigger) land in current_passes but not @passes; accumulate everything
     * that's actually been scheduled so a rewind restores those too -
     * otherwise the pass that triggered the rewind is dropped from the run
     * and never reruns.
     */
    let mut scheduled = passes;
    let mut ret = Ok(());
    let mut prev = NONE;

    while rec.current_passes.get() != 0 {
        scheduled |= rec.current_passes.get();

        let pass = Pass::from_bits(rec.current_passes.get().trailing_zeros());

        let (runner, w) = r.parts();
        w.current_pass.set(pass);
        runner.pass_start_time = ktime_get_ns();
        *w.current_passes.place() &= !bit(pass);
        *w.scheduled_passes_ephemeral.place() &= !bit(pass);
        runner.passes_attempted |= bit(pass);

        drop(r);
        let ret2 = run_pass(fs, pass).and_then(|()| fs.journal_flush());
        r = lock(fs);

        if r.rewound_to != NONE {
            r.rewound_from = r.rewound_from.max(pass);
            // Restore current_passes up to and including rewound_to
            let (runner, w) = r.parts();
            *w.current_passes.place() |= scheduled & (!0u64 << runner.rewound_to as u32);
            runner.rewound_to = NONE;
        } else if ret2.is_ok() {
            let w = r.published();
            w.pass_done.set(w.pass_done.get().max(pass));
            *w.passes_complete.place() |= bit(pass);
        } else {
            ret = ret2;
        }

        if ret.is_err() && failfast {
            break;
        }

        // the point it waits for - see bch2_reconcile_thread()
        let resume_logged_ops_early = Pass::BCH_RECOVERY_PASS_resume_logged_ops_early;
        if prev <= resume_logged_ops_early && pass > resume_logged_ops_early {
            fs.reconcile_wakeup();
        }

        prev = pass;
    }

    r.published().current_pass.set(NONE);
    drop(r);

    fs.clear_flag(c::bch_fs_flags::BCH_FS_running_recovery_passes);
    ret
}

#[c_export]
pub fn bch2_run_recovery_passes(c: &Opaque<c::bch_fs>, passes: u64, failfast: bool) -> c_int {
    ret_to_c(run_passes(Fs::from_c(c), passes, failfast))
}

/// The async runner: the passes scheduled that can run online, unless a run
/// is already going - then it picks them up.
unsafe extern "C" fn async_work(work: *mut c::work_struct) {
    // SAFETY: recovery.work, of a live filesystem: run_async() queued it,
    // handing it a write ref
    let fs = unsafe { Fs::borrow_raw(work.byte_sub(offset_of!(c::bch_fs, recovery.work)).cast()) };
    let _ref = unsafe { WriteRef::adopt(fs, c::bch_write_ref::BCH_WRITE_REF_async_recovery_passes) };

    let Some(_run) = fs.recovery.run_lock.try_lock() else { return };

    /*
     * A persistent pass is recorded by the same sb_lock section that kicks
     * us, and only reaches c->sb when that superblock write finishes: read
     * it under sb_lock, or we can run before the request is visible and
     * never see it.
     */
    let required = {
        let _sb = fs.sb_lock();
        fs.sb.recovery_passes_required
    };

    let ratelimiting = lock(fs).passes_ratelimiting;
    let passes = (required | fs.recovery.runner.scheduled_passes_ephemeral.get()) & !ratelimiting & passes_match(c::PASS_ONLINE);
    let _ = run_passes(fs, passes, false);
}

/// Kick the async runner, if the filesystem is writable: for passes that can
/// run online. Nothing in it sleeps.
pub fn run_async(fs: &Fs) {
    /*
     * A nochanges mount goes fake-rw during recovery for fsck, then back to
     * ro before completing (we can't hold fake-rw indefinitely - we'd oom),
     * so a background pass would fire when we're no longer rw. Don't schedule
     * them at all.
     */
    if fs.opts().nochanges != 0 {
        return;
    }

    if let Some(r) = fs.write_ref_tryget(c::bch_write_ref::BCH_WRITE_REF_async_recovery_passes) {
        r.queue_work_on(Queue::system_long(), &fs.recovery.work);
    }
}

/// Recovery at mount: the passes this mount needs, from @from on - those that
/// can, deferred to run in the background, after.
pub fn run_passes_startup(fs: &Fs, from: Pass) -> Result<(), BchError> {
    /*
     * We can't allow set_may_go_rw to be excluded; that would cause us to
     * use the journal replay keys for updates where it's not expected.
     */
    // SAFETY: mount's, before anything else reads it
    unsafe { (*fs.as_ptr()).opts.recovery_passes_exclude &= !bit(Pass::BCH_RECOVERY_PASS_set_may_go_rw) };
    let opts = fs.opts();

    lock(fs).published().scheduled_passes_ephemeral.set(opts.recovery_passes);

    /*
     * Scheduled passes are repair the superblock is asking for - an upgrade
     * wanting accounting rebuilt, damage found on a previous mount. Skipping
     * them doesn't clear them: they stay in the superblock and run at the
     * next mount that doesn't skip. For tools that open a filesystem to look
     * at it rather than fix it, where check_allocations rebuilding the world
     * before the first command is the opposite of what was asked for.
     */
    let mut passes = passes_match(c::PASS_ALWAYS)
        | if fs.sb.clean == 0 { passes_match(c::PASS_UNCLEAN) } else { 0 }
        | if opts.fsck != 0 { fsck_passes() } else { 0 }
        | opts.recovery_passes
        | if opts.recovery_passes_skip_scheduled == 0 { fs.sb.recovery_passes_required } else { 0 };

    if opts.recovery_pass_last != 0 {
        passes &= 1u64.checked_shl(u32::from(opts.recovery_pass_last) + 1).map_or(!0, |b| b - 1);
    }

    passes &= !opts.recovery_passes_exclude;

    passes &= !(bit(from) - 1);

    /*
     * Defer passes that can be run online, and don't have dependents that
     * can't be run online
     */
    let mut defer = 0;
    if opts.fsck == 0 {
        for i in 0..NR {
            if passes & 1 << i != 0 &&
               opts.recovery_passes & 1 << i == 0 &&
               WHEN[i] & c::PASS_NODEFER == 0 &&
               should_defer(i, passes) {
                defer |= 1 << i;
                passes &= !(1 << i);
            }
        }
    }

    {
        let _run = run_lock(fs);
        run_passes(fs, passes, true)?;
    }

    fs.clear_flag(c::bch_fs_flags::BCH_FS_in_recovery);

    if defer != 0 {
        let mut msg = LogMsg::with_level(fs, LogLevel::NOTICE);
        write!(msg, "Running the following recovery passes in the background:\n");
        msg.bitflags(&NAMES, defer);

        *lock(fs).published().scheduled_passes_ephemeral.place() |= defer;
    }

    /*
     * Kick the async runner for everything scheduled to run in the
     * background: the defer passes above, and passes scheduled ephemerally
     * during recovery (e.g. delete_dead_snapshots via a trigger), which land
     * in scheduled_passes_ephemeral but not defer. They were scheduled before
     * we went rw, so their own kick was a no-op. (Itself a no-op in
     * nochanges - see run_async().)
     */
    if fs.recovery.runner.scheduled_passes_ephemeral.get() != 0 {
        run_async(fs);
    }

    Ok(())
}

#[c_export]
pub fn bch2_run_recovery_passes_startup(c: &Opaque<c::bch_fs>, from: c::bch_recovery_pass) -> c_int {
    ret_to_c(run_passes_startup(Fs::from_c(c), from))
}

// ── Status ───────────────────────────────────────────────────────────────────

/// Each pass's state - scheduled, completed, failing - and the running one's
/// progress.
#[cold]
pub fn status_to_text(fs: &Fs, out: &mut Printbuf) {
    let r = &fs.recovery;
    // the runner's, copied out: printing allocates, which it can't under the lock
    let (failing, start_time, rewound_from) = {
        let runner = lock(fs);
        (runner.passes_failing, runner.pass_start_time, runner.rewound_from)
    };
    let line = |out: &mut Printbuf, what: &str, passes: u64| {
        write!(out, "{what}:\t");
        out.bitflags(&NAMES, passes);
        out.newline();
    };

    out.tabstop_push(32);
    line(out, "Scheduled (superblock)", fs.sb.recovery_passes_required);
    line(out, "Scheduled (ephemeral)", r.runner.scheduled_passes_ephemeral.get());

    line(out, "Completed", r.runner.passes_complete.get());
    line(out, "Failing", failing);

    let current = r.runner.current_pass.get();
    if current != NONE {
        write!(out, "Currently running:\t{} ({})\n", current.name(), current as u32);

        write!(out, "Elapsed:\t");
        out.time_units(ktime_get_ns().wrapping_sub(start_time));
        out.newline();

        // Nothing to show for a pass with no estimate of its own size
        if crate::init::progress::bch2_progress_has_total(&r.progress) {
            write!(out, "Progress:\t");
            prt!(out, "{}", r.progress);
            out.newline();
        }

        line(out, "Next", r.runner.current_passes.get());

        if fs.flag(c::bch_fs_flags::BCH_FS_in_recovery) && rewound_from != NONE {
            write!(out, "Rewound from:\t{} ({})\n", rewound_from.name(), rewound_from as u32);
        }
    }
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

        let check_topology = Pass::BCH_RECOVERY_PASS_check_topology;
        assert_eq!((check_topology.to_stable().0, check_topology.name()), (4, "check_topology"));
    }
}
