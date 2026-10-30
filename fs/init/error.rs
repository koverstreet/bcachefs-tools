// SPDX-License-Identifier: GPL-2.0

//! fsck errors, as C's fsck_err() family (init/error.h).
//!
//! __bch2_fsck_err() decides what to do about an inconsistency - fix it,
//! ignore it, ask, or give up - and answers with an errcode: fsck_fix,
//! fsck_ignore, or a real error (fsck_errors_not_fixed,
//! fsck_repair_unimplemented, ...). Never 0. The C macros turn that into a
//! bool and a `goto fsck_err`; here it's Result<bool>, and the `?` is the goto:
//!
//! ```ignore
//! if fsck_err_on!(trans, nlink != count, id::inode_wrong_nlink,
//!                 "inode has wrong i_nlink ({}, should be {})", nlink, count)? {
//!     repair
//! }
//! ```
//!
//! The work is in plain functions, the message a &dyn ToText - not written
//! when there's no error to report; the macros of the same names make the
//! message with message!() - a format string, or a ToText - so call sites
//! read as C's do. The message goes down as "%s": prefixing, ratelimiting and
//! the superblock error counters stay __bch2_fsck_err()'s.

use crate::btree::bkey::{BkeySC, POS_MIN};
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::{bch_fsck_flags, bch_sb_error_id};
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::prt;
use crate::sb::members::DevOuterRef;
use crate::util::ffi::{c_char, Opaque};
use crate::util::kernel::{jiffies, time_after, HZ};
use crate::util::log::{log_msg_start, print_str, LogLevel, LogMsg};
use crate::util::workqueue::Queue;
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_ulong, CStr};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_export;

/// The fsck error ids by their BCH_SB_ERRS() names: id::inode_wrong_nlink.
pub mod id {
    use crate::c::{self, bch_sb_error_id};

    macro_rules! fsck_err_ids {
        ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { ::paste::paste! { $(
            #[allow(non_upper_case_globals)]
            pub const $name: bch_sb_error_id = bch_sb_error_id::[<BCH_FSCK_ERR_ $name>];
        )* } };
    }
    c::BCH_SB_ERRS!(fsck_err_ids);
}

/// What an fsck error is reported against. A transaction is passed down so
/// __bch2_fsck_err() can drop its locks while it waits on a question; an error
/// reported against just the filesystem must not have one held.
pub trait FsckErrCtx {
    fn fsck_err_ptrs(&self) -> (*mut c::bch_fs, *mut c::btree_trans);
}

impl FsckErrCtx for Fs {
    fn fsck_err_ptrs(&self) -> (*mut c::bch_fs, *mut c::btree_trans) {
        crate::warn_on!(unsafe { c::bch2_current_has_btree_trans(self.as_ptr()) });
        (self.as_ptr(), core::ptr::null_mut())
    }
}

impl FsckErrCtx for BtreeTrans<'_> {
    fn fsck_err_ptrs(&self) -> (*mut c::bch_fs, *mut c::btree_trans) {
        (core::ptr::null_mut(), self.raw())
    }
}

impl FsckErrCtx for TransAttempt<'_, '_> {
    fn fsck_err_ptrs(&self) -> (*mut c::bch_fs, *mut c::btree_trans) {
        (core::ptr::null_mut(), self.raw())
    }
}

fn __fsck_err(
    ctx:   &impl FsckErrCtx,
    pos:   c::bpos,
    flags: bch_fsck_flags,
    err:   bch_sb_error_id,
    msg:   &dyn ToText,
) -> Result<bool, BchError> {
    let mut buf = Printbuf::new();
    msg.to_text(&mut buf);

    let (fs, trans) = ctx.fsck_err_ptrs();
    let ret = unsafe {
        c::__bch2_fsck_err(fs, trans, pos, flags, err,
                           c"%s".as_ptr().cast(), buf.as_raw().buf)
    };
    let e = BchError::from_raw(-ret);

    if e.matches(bch_errcode::BCH_ERR_fsck_fix) {
        Ok(true)
    } else if e.matches(bch_errcode::BCH_ERR_fsck_ignore) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Fixable or ignorable: Ok(true) to fix, Ok(false) to leave it, Err to stop.
pub fn fsck_err(
    ctx: &impl FsckErrCtx,
    err: bch_sb_error_id,
    msg: &dyn ToText,
) -> Result<bool, BchError> {
    __fsck_err(ctx, POS_MIN,
               bch_fsck_flags::FSCK_CAN_FIX | bch_fsck_flags::FSCK_CAN_IGNORE, err, msg)
}

pub fn fsck_err_on(
    ctx:  &impl FsckErrCtx,
    cond: bool,
    err:  bch_sb_error_id,
    msg:  &dyn ToText,
) -> Result<bool, BchError> {
    if cond { fsck_err(ctx, err, msg) } else { Ok(false) }
}

/// Fixable, not ignorable.
pub fn mustfix_fsck_err(
    ctx: &impl FsckErrCtx,
    err: bch_sb_error_id,
    msg: &dyn ToText,
) -> Result<bool, BchError> {
    __fsck_err(ctx, POS_MIN, bch_fsck_flags::FSCK_CAN_FIX, err, msg)
}

pub fn mustfix_fsck_err_on(
    ctx:  &impl FsckErrCtx,
    cond: bool,
    err:  bch_sb_error_id,
    msg:  &dyn ToText,
) -> Result<bool, BchError> {
    if cond { mustfix_fsck_err(ctx, err, msg) } else { Ok(false) }
}

/// Against an inode: @pos, its (0, inum, snapshot), is where __bch2_fsck_err()
/// records the damage - which takes a transaction.
pub fn inode_fsck_err(
    t:   &TransAttempt<'_, '_>,
    pos: c::bpos,
    err: bch_sb_error_id,
    msg: &dyn ToText,
) -> Result<bool, BchError> {
    __fsck_err(t, pos,
               bch_fsck_flags::FSCK_CAN_FIX | bch_fsck_flags::FSCK_CAN_IGNORE, err, msg)
}

/// For a caller that decides whether the error can be fixed. @pos is POS_MIN
/// for none.
pub fn fsck_err_flags(
    ctx:   &impl FsckErrCtx,
    pos:   c::bpos,
    flags: bch_fsck_flags,
    err:   bch_sb_error_id,
    msg:   &dyn ToText,
) -> Result<bool, BchError> {
    __fsck_err(ctx, pos, flags, err, msg)
}

/// Logged and counted, ignorable, with no repair offered - the caller
/// repairs, or carries on, whatever's decided: C's log_fsck_err(). Its
/// error, if fsck stops there.
pub fn log_fsck_err(ctx: &impl FsckErrCtx, err: bch_sb_error_id, msg: &dyn ToText) -> Result<(), BchError> {
    __fsck_err(ctx, POS_MIN, bch_fsck_flags::FSCK_CAN_IGNORE, err, msg).map(|_| ())
}

/// Without offering a repair, for damage another pass owns: counted in the
/// superblock, and logged unless it's a repeat or being ratelimited.
pub fn fsck_err_report(fs: &Fs, err: bch_sb_error_id, msg: &dyn ToText) {
    let mut buf = LogMsg::new(fs);
    msg.to_text(&mut buf);

    let log = count_fsck_err(fs, err, &mut buf);
    buf.set_suppressed(!log);
}

/// Whether to log @msg: not if it's a repeat or being ratelimited. If this is
/// where ratelimiting starts, @msg says so.
pub fn count_fsck_err(fs: &Fs, err: bch_sb_error_id, msg: &mut Printbuf) -> bool {
    unsafe { c::__bch2_count_fsck_err(fs.as_ptr(), err, msg.as_raw()) }
}

pub fn sb_error_count(fs: &Fs, err: bch_sb_error_id) {
    fs.sb_error_count(err)
}

/// fs_inconsistent(), with what the transaction has queued in the message.
pub fn trans_inconsistent(t: &TransAttempt<'_, '_>, msg: &dyn ToText) -> bool {
    fs_trans_inconsistent(t.fs(), Some(t), msg)
}

/// Acts as the errors option says - continue, go emergency read-only, or
/// panic - and returns whether we're going read-only. From atomic context
/// too, as C's.
pub fn fs_inconsistent(fs: &Fs, msg: &dyn ToText) -> bool {
    fs_trans_inconsistent(fs, None, msg)
}

/// Go emergency read-only, logging @msg as a fatal error in @func - if this
/// is what took us read-only.
pub fn fs_fatal_error(fs: &Fs, func: &CStr, msg: &dyn ToText) {
    let mut m = LogMsg::new(fs);
    m.set_suppressed(true);

    prt!(&mut m, "{}(): fatal error ", func.to_str().unwrap_or("?"));
    msg.to_text(&mut m);
    m.newline();

    fs.emergency_read_only(&mut m);
    prt!(&mut m, "fatal error - emergency read only");
}

/// What C's bkey_fsck_err() finds by name in a key type's validate: c, k,
/// from.
pub struct BkeyValidate<'a, 'k> {
    pub fs:   &'a Fs,
    pub k:    BkeySC<'k>,
    pub from: &'a c::bkey_validate_context,
}

/// The error validate returns: fsck_delete_bkey if the key is to be dropped -
/// fsck said fix or ignore, or the error is silenced - else what stops us.
pub fn bkey_fsck_err(
    v:   &BkeyValidate<'_, '_>,
    err: bch_sb_error_id,
    msg: &dyn ToText,
) -> BchError {
    let mut buf = Printbuf::new();
    msg.to_text(&mut buf);

    let ret = unsafe {
        c::__bch2_bkey_fsck_err(v.fs.as_ptr(), v.k.into(), v.from, err,
                                c"%s".as_ptr().cast(), buf.as_raw().buf)
    };
    let e = BchError::from_raw(-ret);

    if e.matches(bch_errcode::BCH_ERR_fsck_fix) || e.matches(bch_errcode::BCH_ERR_fsck_ignore) {
        v.fs.err(bch_errcode::BCH_ERR_fsck_delete_bkey)
    } else {
        e
    }
}

pub fn bkey_fsck_err_on(
    v:    &BkeyValidate<'_, '_>,
    cond: bool,
    err:  bch_sb_error_id,
    msg:  &dyn ToText,
) -> Result<(), BchError> {
    if cond { Err(bkey_fsck_err(v, err, msg)) } else { Ok(()) }
}

// ── Filesystem errors ────────────────────────────────────────────────────

impl Fs {
    /// An inconsistency, acted on as the errors option says - carry on, go
    /// emergency read-only, or panic - and noted in @out: whether we're going
    /// read-only.
    pub fn inconsistent_error(&self, out: &mut Printbuf) -> bool {
        use c::bch_error_actions as a;

        self.set_flag(c::bch_fs_flags::BCH_FS_error);
        match self.opts.errors() {
            a::BCH_ON_ERROR_continue => false,
            a::BCH_ON_ERROR_fix_safe | a::BCH_ON_ERROR_ro => {
                self.emergency_read_only(out);
                true
            }
            a::BCH_ON_ERROR_panic => {
                print_str(self, LogLevel::ERR, out);
                panic_after_error(self)
            }
            e => panic!("errors option {}", e.0),
        }
    }

    /// A btree topology error, noted in @out - an inconsistency, or in
    /// recovery, check_topology run explicitly: the error to stop on, the
    /// pass's or btree_need_topology_repair.
    pub fn topology_error(&self, out: &mut Printbuf) -> BchError {
        write!(out, "btree topology error: ");

        self.set_flag(c::bch_fs_flags::BCH_FS_topology_error);
        if !self.flag(c::bch_fs_flags::BCH_FS_in_recovery) {
            self.inconsistent_error(out);
        } else if let Err(e) = crate::init::passes::run_explicit(self, out,
                c::bch_recovery_pass::BCH_RECOVERY_PASS_check_topology,
                c::bch_run_recovery_pass_flags::from_bits_retain(0)) {
            return e;
        }
        self.err(bch_errcode::BCH_ERR_btree_need_topology_repair)
    }
}

fn panic_after_error(fs: &Fs) -> ! {
    #[cfg(kernel)]
    {
        let mut msg = Printbuf::new();
        write!(msg, "bcachefs ({}): panic after error\n", fs.name().to_str().unwrap_or("?"));
        // SAFETY: a format, and the string it takes
        unsafe { c::panic(c"%s".as_ptr().cast(), msg.as_c_str().as_ptr()) }
    }
    #[cfg(not(kernel))]
    panic!("{}: panic after error", fs.name().to_str().unwrap_or("?"))
}

/// The inconsistency @msg says - in @t, whose queued updates go with it -
/// acted on, and printed: whether we're going read-only. From atomic context
/// too: the message doesn't sleep to grow.
fn fs_trans_inconsistent(fs: &Fs, t: Option<&TransAttempt<'_, '_>>, msg: &dyn ToText) -> bool {
    let mut buf = Printbuf::new();
    let mut out = buf.atomic();

    log_msg_start(&mut out, fs.name());
    msg.to_text(&mut out);
    out.newline();

    if let Some(t) = t {
        t.updates_to_text(&mut out);
    }
    let ret = fs.inconsistent_error(&mut out);
    print_str(fs, LogLevel::ERR, &mut out);
    ret
}

#[c_export]
pub fn __bch2_inconsistent_error(c: &Opaque<c::bch_fs>, out: &mut Printbuf) -> bool {
    Fs::from_c(c).inconsistent_error(out)
}

/// An inconsistency, with nothing to say about it.
#[c_export]
pub fn bch2_inconsistent_error(c: &Opaque<c::bch_fs>) -> bool {
    let fs = Fs::from_c(c);
    let mut buf = Printbuf::new();
    let mut out = buf.atomic();
    out.indent_add_nextline(2);

    let ret = fs.inconsistent_error(&mut out);
    if ret {
        crate::bch_err!(fs, "{}", out.as_str());
    }
    ret
}

/// # Safety
/// @trans is NULL, or in an attempt. For error.c's
/// bch2_fs_inconsistent() and bch2_trans_inconsistent().
#[c_export]
pub unsafe fn bch2_fs_trans_inconsistent_str(c: &Opaque<c::bch_fs>, trans: Option<&BtreeTrans<'_>>,
                                             msg: &Printbuf) -> bool {
    // SAFETY: the caller's
    let t = trans.map(|t| unsafe { t.attempt_in_progress() });
    fs_trans_inconsistent(Fs::from_c(c), t.as_ref(), msg)
}

#[c_export]
pub fn __bch2_topology_error(c: &Opaque<c::bch_fs>, out: &mut Printbuf) -> c_int {
    ret_to_c(Err::<(), _>(Fs::from_c(c).topology_error(out)))
}

/// # Safety
/// @func is a string. For error.c's bch2_fatal_error().
#[c_export]
pub unsafe fn bch2_fatal_error_str(c: &Opaque<c::bch_fs>, func: *const c_char, msg: &Printbuf) {
    // SAFETY: the caller's
    fs_fatal_error(Fs::from_c(c), unsafe { CStr::from_ptr(func.cast()) }, msg)
}

// ── IO errors ────────────────────────────────────────────────────────────
//
// A device's IO errors are counted as they happen, and writes failing for
// write_error_timeout set it read-only - the filesystem, if the device can't
// go. That's a work item's, as it takes state_lock, which device removal
// holds while it drains ca->ref: so the work holds a ref_outer - the
// device's memory only - and rechecks under the lock that it's not being
// removed, nor the filesystem stopping.

impl c::bch_dev {
    /// An IO error of type @ty: counted, and a write's starting the write
    /// error clock, which io_error_work() reads.
    pub fn io_error(&self, ty: c::bch_member_error_type) {
        self.errors[ty.0 as usize].fetch_add(1, Relaxed);

        if ty == c::bch_member_error_type::BCH_MEMBER_ERROR_write && self.write_errors_start.load(Relaxed) == 0 {
            self.write_errors_start.store(jiffies() as usize, Relaxed);
        }

        // a ref per queued instance
        let r = self.outer_ref();
        // SAFETY: the device's work, initialized with it, which the ref
        // keeps until it runs
        if unsafe { Queue::system_long().queue_work((&raw const self.io_error_work).cast_mut()) } {
            let _ = r.into_raw();
        }
    }
}

/// # Safety
/// @work is a bch_dev's io_error_work, queued by io_error() with an outer ref.
#[c_export]
pub unsafe extern "C" fn bch2_io_error_work(work: *mut c::work_struct) {
    // SAFETY: the caller's - and the ref, ours to put
    let ca = unsafe {
        DevOuterRef::from_raw(work.byte_sub(core::mem::offset_of!(c::bch_dev, io_error_work)).cast())
    };
    let fs = ca.fs();

    // XXX: if it's reads or checksums that are failing, set it to failed
    let start = ca.write_errors_start.load(Relaxed) as c_ulong;
    let timeout = c_ulong::from(fs.opts.write_error_timeout.get());
    if start == 0 || !time_after(jiffies(), start.wrapping_add(timeout * HZ)) {
        return;
    }

    let state = fs.state_lock_write();
    if ca.removing() ||
       fs.flag(c::bch_fs_flags::BCH_FS_stopping) ||
       ca.mi.get().state >= c::BCH_MEMBER_STATE_ro as u8 {
        return;
    }

    let mut buf = Printbuf::new();
    log_msg_start(&mut buf, ca.name());
    write!(buf, "writes erroring for {timeout} seconds\n");

    let dev = state.dev_set_state(&ca, c::bch_member_state::BCH_MEMBER_STATE_ro,
                                  c::BCH_FORCE_IF_DEGRADED, &mut buf).is_ok();
    write!(buf, "setting {} ro", if dev { "device" } else { "filesystem" });
    if dev || fs.emergency_read_only(&mut buf) {
        print_str(fs, LogLevel::ERR, &mut buf);
    }
}

#[c_export]
pub fn bch2_io_error(ca: &c::bch_dev, ty: c::bch_member_error_type) {
    ca.io_error(ty)
}

// ── Macros: the functions above, with message!() written for you ──────────

#[macro_export]
macro_rules! bkey_fsck_err {
    ($v:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::bkey_fsck_err($v, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! bkey_fsck_err_on {
    ($v:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::bkey_fsck_err_on($v, $cond, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! fsck_err {
    ($ctx:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::fsck_err($ctx, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! fsck_err_on {
    ($ctx:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::fsck_err_on($ctx, $cond, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! mustfix_fsck_err {
    ($ctx:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::mustfix_fsck_err($ctx, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! mustfix_fsck_err_on {
    ($ctx:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::mustfix_fsck_err_on($ctx, $cond, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! fsck_err_flags {
    ($ctx:expr, $pos:expr, $flags:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::fsck_err_flags($ctx, $pos, $flags, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! log_fsck_err {
    ($ctx:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::log_fsck_err($ctx, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! fsck_err_report {
    ($fs:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::fsck_err_report($fs, $err, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! trans_inconsistent {
    ($trans:expr, $($msg:tt)*) => {
        $crate::init::error::trans_inconsistent($trans, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! fs_inconsistent {
    ($fs:expr, $($msg:tt)*) => {
        $crate::init::error::fs_inconsistent($fs, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! fs_fatal_error {
    ($fs:expr, $($msg:tt)*) => {
        $crate::init::error::fs_fatal_error($fs, $crate::c_function_name!(), $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! inode_fsck_err {
    ($trans:expr, $pos:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::inode_fsck_err($trans, $pos, $err, $crate::message!($($msg)*))
    };
}
