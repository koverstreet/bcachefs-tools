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
//! format_args!() is lazy, so the work is in plain functions; the macros of
//! the same names only write the format_args!(), so call sites read as C's
//! do. The message goes down as "%s": prefixing, ratelimiting and the
//! superblock error counters stay __bch2_fsck_err()'s.

use crate::btree::bkey::{BkeySC, POS_MIN};
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::{bch_fsck_flags, bch_sb_error_id};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::util::Printbuf;
use core::fmt;

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
        crate::warn_on!(unsafe { c::bch2_current_has_btree_trans(self.raw) });
        (self.raw, core::ptr::null_mut())
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
    msg:   fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    let mut buf = Printbuf::new();
    buf.write_fmt(msg);

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
    msg: fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    __fsck_err(ctx, POS_MIN,
               bch_fsck_flags::FSCK_CAN_FIX | bch_fsck_flags::FSCK_CAN_IGNORE, err, msg)
}

pub fn fsck_err_on(
    ctx:  &impl FsckErrCtx,
    cond: bool,
    err:  bch_sb_error_id,
    msg:  fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    if cond { fsck_err(ctx, err, msg) } else { Ok(false) }
}

/// Fixable, not ignorable.
pub fn mustfix_fsck_err(
    ctx: &impl FsckErrCtx,
    err: bch_sb_error_id,
    msg: fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    __fsck_err(ctx, POS_MIN, bch_fsck_flags::FSCK_CAN_FIX, err, msg)
}

pub fn mustfix_fsck_err_on(
    ctx:  &impl FsckErrCtx,
    cond: bool,
    err:  bch_sb_error_id,
    msg:  fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    if cond { mustfix_fsck_err(ctx, err, msg) } else { Ok(false) }
}

/// Against an inode: @pos, its (0, inum, snapshot), is where __bch2_fsck_err()
/// records the damage - which takes a transaction.
pub fn inode_fsck_err(
    t:   &TransAttempt<'_, '_>,
    pos: c::bpos,
    err: bch_sb_error_id,
    msg: fmt::Arguments<'_>,
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
    msg:   fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    __fsck_err(ctx, pos, flags, err, msg)
}

/// Without offering a repair, for damage another pass owns: counted in the
/// superblock, and logged unless it's a repeat or being ratelimited.
pub fn fsck_err_report(fs: &Fs, err: bch_sb_error_id, msg: fmt::Arguments<'_>) {
    let mut buf = Printbuf::new();
    buf.write_fmt(msg);

    if count_fsck_err(fs, err, &mut buf) {
        crate::bch_err!(fs, "{}", buf);
    }
}

/// Whether to log @msg: not if it's a repeat or being ratelimited. If this is
/// where ratelimiting starts, @msg says so.
pub fn count_fsck_err(fs: &Fs, err: bch_sb_error_id, msg: &mut Printbuf) -> bool {
    unsafe { c::__bch2_count_fsck_err(fs.raw, err, msg.as_raw()) }
}

pub fn sb_error_count(fs: &Fs, err: bch_sb_error_id) {
    unsafe { c::bch2_sb_error_count(fs.raw, err) }
}

/// fs_inconsistent(), with what the transaction has queued in the message.
pub fn trans_inconsistent(t: &TransAttempt<'_, '_>, msg: fmt::Arguments<'_>) -> bool {
    let mut buf = Printbuf::new();
    buf.write_fmt(msg);

    unsafe { c::bch2_trans_inconsistent(t.raw(), c"%s".as_ptr().cast(), buf.as_raw().buf) }
}

/// Acts as the errors option says - continue, go emergency read-only, or
/// panic - and returns whether we're going read-only. Unlike C's, not from
/// atomic context: formatting the message allocates, and can sleep.
pub fn fs_inconsistent(fs: &Fs, msg: fmt::Arguments<'_>) -> bool {
    let mut buf = Printbuf::new();
    buf.write_fmt(msg);

    unsafe { c::bch2_fs_inconsistent(fs.raw, c"%s".as_ptr().cast(), buf.as_raw().buf) }
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
    msg: fmt::Arguments<'_>,
) -> BchError {
    let mut buf = Printbuf::new();
    buf.write_fmt(msg);

    let ret = unsafe {
        c::__bch2_bkey_fsck_err(v.fs.raw, v.k.to_raw(), v.from, err,
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
    msg:  fmt::Arguments<'_>,
) -> Result<(), BchError> {
    if cond { Err(bkey_fsck_err(v, err, msg)) } else { Ok(()) }
}

// ── Macros: the functions above, with format_args!() written for you ──────

#[macro_export]
macro_rules! bkey_fsck_err {
    ($v:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::bkey_fsck_err($v, $err, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! bkey_fsck_err_on {
    ($v:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::bkey_fsck_err_on($v, $cond, $err, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! fsck_err {
    ($ctx:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::fsck_err($ctx, $err, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! fsck_err_on {
    ($ctx:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::fsck_err_on($ctx, $cond, $err, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! mustfix_fsck_err {
    ($ctx:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::mustfix_fsck_err($ctx, $err, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! mustfix_fsck_err_on {
    ($ctx:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::mustfix_fsck_err_on($ctx, $cond, $err, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! fsck_err_flags {
    ($ctx:expr, $pos:expr, $flags:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::fsck_err_flags($ctx, $pos, $flags, $err, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! fsck_err_report {
    ($fs:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::fsck_err_report($fs, $err, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! trans_inconsistent {
    ($trans:expr, $($msg:tt)*) => {
        $crate::init::error::trans_inconsistent($trans, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! fs_inconsistent {
    ($fs:expr, $($msg:tt)*) => {
        $crate::init::error::fs_inconsistent($fs, format_args!($($msg)*))
    };
}

#[macro_export]
macro_rules! inode_fsck_err {
    ($trans:expr, $pos:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::inode_fsck_err($trans, $pos, $err, format_args!($($msg)*))
    };
}
