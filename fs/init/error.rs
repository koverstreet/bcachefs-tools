// SPDX-License-Identifier: GPL-2.0

//! fsck errors, as C's fsck_err() family (init/error.h).
//!
//! __bch2_fsck_err() decides what to do about an inconsistency - fix it,
//! ignore it, ask, or give up - and answers with an errcode: fsck_fix,
//! fsck_ignore, or a real error (fsck_errors_not_fixed,
//! fsck_repair_unimplemented, ...). Never 0. The C macros turn that into a
//! bool and a `goto fsck_err`; here it's Result<bool>, and the `?` is the goto:
//!
//!	if fsck_err_on!(trans, nlink != count, id::inode_wrong_nlink,
//!			"inode has wrong i_nlink ({}, should be {})", nlink, count)? {
//!	    repair
//!	}
//!
//! format_args!() is lazy - nothing is formatted unless the error is
//! reported - so the work is in plain functions taking it; C needed macros
//! for that. The macros of the same names only write the format_args!(), so
//! call sites read as C's do. The message is passed down as "%s":
//! prefixing, ratelimiting and the superblock error counters stay
//! __bch2_fsck_err()'s, as for C callers.

use crate::btree::bkey::POS_MIN;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::{bch_fsck_flags, bch_sb_error_id};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::util::Printbuf;
use core::fmt;

/// The fsck error ids by their BCH_SB_ERRS() names: id::inode_wrong_nlink.
pub mod id {
    use crate::c::bch_sb_error_id;

    include!(concat!(env!("OUT_DIR"), "/fsck_err_ids_gen.rs"));
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
                           c"%s".as_ptr(), buf.as_raw().buf)
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

/// Report @err, fixable or ignorable, as C's fsck_err(): Ok(true) to fix,
/// Ok(false) to leave it, Err to stop.
pub fn fsck_err(
    ctx: &impl FsckErrCtx,
    err: bch_sb_error_id,
    msg: fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    __fsck_err(ctx, POS_MIN,
               bch_fsck_flags::FSCK_CAN_FIX | bch_fsck_flags::FSCK_CAN_IGNORE, err, msg)
}

/// fsck_err() if @cond holds, as C's fsck_err_on().
pub fn fsck_err_on(
    ctx:  &impl FsckErrCtx,
    cond: bool,
    err:  bch_sb_error_id,
    msg:  fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    if cond { fsck_err(ctx, err, msg) } else { Ok(false) }
}

/// Report @err, fixable but not ignorable, as C's mustfix_fsck_err().
pub fn mustfix_fsck_err(
    ctx: &impl FsckErrCtx,
    err: bch_sb_error_id,
    msg: fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    __fsck_err(ctx, POS_MIN, bch_fsck_flags::FSCK_CAN_FIX, err, msg)
}

/// mustfix_fsck_err() if @cond holds, as C's mustfix_fsck_err_on().
pub fn mustfix_fsck_err_on(
    ctx:  &impl FsckErrCtx,
    cond: bool,
    err:  bch_sb_error_id,
    msg:  fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    if cond { mustfix_fsck_err(ctx, err, msg) } else { Ok(false) }
}

/// fsck_err() against an inode, as C's inode_fsck_err(): @pos, the inode's
/// (0, inum, snapshot), is where __bch2_fsck_err() records the damage - which
/// takes a transaction.
pub fn inode_fsck_err(
    trans: &BtreeTrans<'_>,
    pos:   c::bpos,
    err:   bch_sb_error_id,
    msg:   fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    __fsck_err(trans, pos,
               bch_fsck_flags::FSCK_CAN_FIX | bch_fsck_flags::FSCK_CAN_IGNORE, err, msg)
}

/// fsck_err() with the caller's @flags, against @pos - POS_MIN for none: as
/// C's __fsck_err() and __inode_fsck_err(), for a caller that decides
/// whether the error can be fixed.
pub fn fsck_err_flags(
    ctx:   &impl FsckErrCtx,
    pos:   c::bpos,
    flags: bch_fsck_flags,
    err:   bch_sb_error_id,
    msg:   fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    __fsck_err(ctx, pos, flags, err, msg)
}

/// Report @err without offering a repair - for damage another pass owns - as
/// C's bch_log_msg with bch2_count_fsck_err(): counted in the superblock, and
/// logged unless it's a repeat or being ratelimited.
pub fn fsck_err_report(fs: &Fs, err: bch_sb_error_id, msg: fmt::Arguments<'_>) {
    let mut buf = Printbuf::new();
    buf.write_fmt(msg);

    if unsafe { c::__bch2_count_fsck_err(fs.raw, err, buf.as_raw()) } {
        crate::bch_err!(fs, "{}", buf);
    }
}

// The family as macros taking a format string, as C's do, so call sites read
// like C's: the functions above, with the message's format_args!() written
// for you.

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
macro_rules! inode_fsck_err {
    ($trans:expr, $pos:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::inode_fsck_err($trans, $pos, $err, format_args!($($msg)*))
    };
}
