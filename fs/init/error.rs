// SPDX-License-Identifier: GPL-2.0

//! fsck errors, as C's fsck_err() family (init/error.h).
//!
//! __bch2_fsck_err() decides what to do about an inconsistency - fix it,
//! ignore it, ask, or give up - and answers with an errcode: fsck_fix,
//! fsck_ignore, or a real error (fsck_errors_not_fixed,
//! fsck_repair_unimplemented, ...). Never 0. The C macros turn that into a
//! bool and a `goto fsck_err`; here it's Result<bool>, and the `?` is the goto:
//!
//!	if fsck_err_on(trans, nlink != count, id::inode_wrong_nlink,
//!		       format_args!("..."))? {
//!	    repair
//!	}
//!
//! format_args!() is lazy - nothing is formatted unless the error is
//! reported - so these are plain functions; C needed macros for that. The
//! message is passed down as "%s": prefixing, ratelimiting and the superblock
//! error counters stay __bch2_fsck_err()'s, as for C callers.

use crate::btree::bkey::POS_MIN;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::{bch_fsck_flags, bch_sb_error_id};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::util::Printbuf;
use core::fmt::{self, Write};

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
    flags: bch_fsck_flags,
    err:   bch_sb_error_id,
    msg:   fmt::Arguments<'_>,
) -> Result<bool, BchError> {
    let mut buf = Printbuf::new();
    let _ = buf.write_fmt(msg);

    let (fs, trans) = ctx.fsck_err_ptrs();
    let ret = unsafe {
        c::__bch2_fsck_err(fs, trans, POS_MIN, flags, err,
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
    __fsck_err(ctx, bch_fsck_flags::FSCK_CAN_FIX | bch_fsck_flags::FSCK_CAN_IGNORE, err, msg)
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
    __fsck_err(ctx, bch_fsck_flags::FSCK_CAN_FIX, err, msg)
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
