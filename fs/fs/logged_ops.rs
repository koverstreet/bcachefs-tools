// SPDX-License-Identifier: GPL-2.0

//! Logged operations: an operation too big for one transaction - a truncate,
//! an finsert, a stripe update, an inode option propagation - keeps a key in
//! BTREE_ID_logged_ops while it runs, so that if we crash part way, recovery
//! can resume it. The ops are in io_misc.rs, ec/create.c and inode_opts.rs;
//! this is starting and finishing one, and recovery resuming what's left.

use crate::btree::bkey::{pos, BkeyI, BkeySC, POS_MIN};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts,
                         TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use crate::{fs_fatal_error, fsck_err_on};
use core::ffi::{c_int, c_uint};
use core::sync::atomic::Ordering;
use cstruct_macros::c_export;

/// Where the ops are, in BTREE_ID_logged_ops: at offsets of this inode.
const INUM: u64 = c::logged_ops_inums::LOGGED_OPS_INUM_logged_ops.0 as u64;

struct LoggedOp {
    /// As written to the sysfs knob
    name:   &'static str,
    ty:     c::bch_bkey_type,
    /// Background work that runs during recovery (copygc, reconcile) can
    /// start these, so they're resumed by resume_logged_ops_early. The rest
    /// are only started from userspace, and are resumed after fsck has
    /// repaired what they walk. Either pass resumes only the ops from before
    /// this mount: see bch2_logged_ops_note_unfinished().
    early:  bool,
    resume: fn(&BtreeTrans<'_>, &mut BkeyI) -> Result<(), BchError>,
}

const LOGGED_OPS: [LoggedOp; 4] = {
    type T = c::bch_bkey_type;
    [
        LoggedOp { name: "truncate", ty: T::KEY_TYPE_logged_op_truncate, early: false,
                   resume: crate::data::io_misc::resume_truncate },
        LoggedOp { name: "finsert", ty: T::KEY_TYPE_logged_op_finsert, early: false,
                   resume: crate::data::io_misc::resume_finsert },
        LoggedOp { name: "stripe_update", ty: T::KEY_TYPE_logged_op_stripe_update, early: true,
                   resume: |trans, k| ret_to_result_void(unsafe {
                       c::bch2_resume_logged_op_stripe_update(trans.raw(), k.as_ffi_mut())
                   }) },
        LoggedOp { name: "inode_opt_propagate", ty: T::KEY_TYPE_logged_op_inode_opt_propagate,
                   early: false, resume: crate::inode_opts::resume_propagate },
    ]
};

fn logged_op(ty: c::bch_bkey_type) -> Option<&'static LoggedOp> {
    LOGGED_OPS.iter().find(|op| op.ty == ty)
}

impl Fs {
    /// Arm the logged_op_fail_next knob with an op's name: the next op of
    /// that type fails once. Empty (or a bare newline) disarms; anything
    /// unrecognised is rejected.
    pub fn logged_op_fail_next_set(&self, s: &str) -> Result<(), BchError> {
        let ty = match s.split([' ', '\t', '\n']).next().unwrap_or_default() {
            ""   => 0,
            name => LOGGED_OPS.iter().find(|op| op.name == name)
                .ok_or(BchError::from(c::EINVAL))?
                .ty.0,
        };

        self.logged_ops.fail_next.store(ty, Ordering::Relaxed);
        Ok(())
    }

    pub fn logged_op_fail_next_to_text(&self, out: &mut Printbuf) {
        let ty = self.logged_ops.fail_next.load(Ordering::Relaxed);
        let name = logged_op(c::bch_bkey_type(ty)).map_or("(none)", |op| op.name);

        let _ = writeln!(out, "{name}");
    }
}

fn resume_logged_op(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, early: bool) -> Result<(), BchError> {
    let fs = t.fs();

    /* Started this mount, and live: see bch2_logged_ops_note_unfinished() */
    let offset = k.k.p.offset;
    if !fs.logged_ops.unfinished.lock().contains(&offset) {
        return Ok(());
    }

    /* Unknown types go with the late pass, which deletes them: */
    let op = logged_op(k.k.key_type());
    if op.map_or(early, |op| op.early != early) {
        return Ok(());
    }

    let mut sk = BkeyBuf::new();
    sk.reassemble(k);

    /*
     * Fixing logs the error to the journal via the transaction: close()
     * commits that before the resume, which runs transactions of its own.
     */
    fsck_err_on!(t, fs.flag(c::bch_fs_flags::BCH_FS_clean_recovery), id::logged_op_but_clean,
                 "filesystem marked as clean but have logged op\n{}", k.to_text(fs))?;

    Err(t.close(CommitFlags::NO_ENOSPC, |trans| {
        if let Some(op) = op {
            let _ = (op.resume)(trans, sk.as_mut());
        }

        /*
         * Not the resume's return - which this deliberately discards.
         * Recovery finishes the op either way, so that an op that can't
         * complete doesn't wedge every subsequent mount.
         */
        finish(trans, sk.as_ref(), Ok(()), CommitOpts::new())
    }))
}

/// Walk the ops - the keys at LOGGED_OPS_INUM_logged_ops - with @f.
fn for_each_logged_op<F>(fs: &Fs, mut f: F) -> Result<(), BchError>
where
    F: FnMut(&TransAttempt<'_, '_>, BkeySC<'_>) -> Result<(), BchError>,
{
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::logged_ops, pos(INUM, 0), BtreeIterFlags::PREFETCH);

    iter.for_each_max(&trans, pos(INUM, u64::MAX), |t, _, k| f(t, k))
}

fn resume_logged_ops(fs: &Fs, early: bool) -> Result<(), BchError> {
    for_each_logged_op(fs, |t, k| resume_logged_op(t, k, early))
}

/*
 * Recovery resumes the logged ops from before this mount. Anything else in the
 * btree by then was started this mount - copygc and reconcile can start stripe
 * updates while recovery is still running - and is live: resuming it would run
 * it twice at once, and finishing it would delete it out from under its owner.
 *
 * So note which ops are there before we go rw, before anything can start one.
 * An op keeps its slot until it's finished, and only recovery finishes these,
 * so a slot noted here still holds the same op when its resume pass comes.
 */
#[c_export]
pub fn bch2_logged_ops_note_unfinished(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);

    ret_to_c(for_each_logged_op(fs, |_, k| {
        Ok(fs.logged_ops.unfinished.lock().push(k.k.p.offset, GFP_KERNEL)?)
    }))
}

crate::recovery_pass!(bch2_resume_logged_ops_early => |fs| resume_logged_ops(fs, true));
crate::recovery_pass!(bch2_resume_logged_ops => |fs| resume_logged_ops(fs, false));

/// Queue @k as a new op, in the first empty slot - where @k's position is
/// set to.
///
/// # Safety
/// @k stays put until the commit.
unsafe fn logged_op_start(t: &TransAttempt<'_, '_>, k: &mut BkeyI) -> Result<(), BchError> {
    let mut iter = BtreeIter::uninit();
    t.bkey_get_empty_slot(&mut iter, c::btree_id::logged_ops, POS_MIN, pos(INUM, u64::MAX))?;

    k.k.p = iter.pos();

    // SAFETY: the caller's - @k stays put until the commit
    unsafe { t.update_key(&iter, k, UpdateTriggerFlags::empty()) }
}

/// Queue @k as a new op in the transaction, in the first empty slot - where
/// @k's position is set to: as __bch2_logged_op_start(), the update a copy
/// in transaction memory, so @k needn't outlive the call.
pub fn start(t: &TransAttempt<'_, '_>, k: &mut BkeyI) -> Result<(), BchError> {
    let mut iter = BtreeIter::uninit();
    t.bkey_get_empty_slot(&mut iter, c::btree_id::logged_ops, POS_MIN, pos(INUM, u64::MAX))?;

    k.k.p = iter.pos();

    t.update(&iter, &t.bkey_copy(k)?, UpdateTriggerFlags::empty())
}

/// Fails once if logged_op_fail_next is armed for @ty, standing in for a crash
/// at this point: as bch2_logged_op_inject_fail(), logged_ops.h's - a copy,
/// while C calls that.
///
/// compare_exchange: arming is by type, so two ops of that type in flight
/// would both fire.
pub fn inject_fail(fs: &Fs, ty: c::bch_bkey_type) -> Result<(), BchError> {
    let knob = &fs.logged_ops.fail_next;

    if knob.load(Ordering::Relaxed) == ty.0 &&
       knob.compare_exchange(ty.0, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        return fs.throw(bch_errcode::BCH_ERR_injected_logged_op_fail);
    }
    Ok(())
}

/// Every op with a cursor advances it through here, so it's the one place to
/// interrupt one: the op is then left at a cursor the real code produced. As
/// bch2_logged_op_update(), logged_ops.h's - a copy, while C calls that.
pub fn update(t: &TransAttempt<'_, '_>, op: &BkeyI) -> Result<(), BchError> {
    inject_fail(t.fs(), op.k.key_type())?;

    t.insert_with(c::btree_id::logged_ops, t.bkey_copy(op)?, BtreeIterFlags::CACHED,
                  UpdateTriggerFlags::empty())
}

/// # Safety
/// @k is a key that stays put until the transaction commits.
#[c_export]
pub unsafe extern "C" fn __bch2_logged_op_start(trans: &BtreeTrans<'_>, k: *mut c::bkey_i) -> c_int {
    let k = unsafe { &mut *BkeyI::from_raw_mut(k) };

    ret_to_c(unsafe { logged_op_start(&trans.attempt_in_progress(), k) })
}

/// # Safety
/// @k is a key.
#[c_export]
pub unsafe extern "C" fn bch2_logged_op_start(trans: &BtreeTrans<'_>, k: *mut c::bkey_i) -> c_int {
    let k = unsafe { &mut *BkeyI::from_raw_mut(k) };

    // SAFETY: @k stays put across the commit_do(): it's the caller's
    ret_to_c(commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| unsafe { logged_op_start(t, k) }))
}

/// Delete op @k, which returned @op_ret: @op_ret, unless the delete fails.
///
/// Callers must hold a write ref (BCH_WRITE_REF_stripe_create,
/// BCH_WRITE_REF_fallocate, VFS sb->s_writers, or recovery context).
///
/// We use no_check_rw because the logged operation has already completed
/// and the cleanup delete must succeed regardless of fs shutdown state —
/// otherwise we get spurious EROFS during umount.
///
/// TODO: encode the write ref requirement in the type system so the compiler
/// enforces it.
///
/// The key goes even when @op_ret is an error: it protects against the process
/// dying, not against the operation failing. The injected failure is the
/// exception - it simulates the process dying, so the op has to survive.
///
/// From @op_ret and not the knob: the knob is one-shot and global, so by now it
/// may have been re-armed or claimed by another op.
pub fn finish(
    trans:  &BtreeTrans<'_>,
    k:      &BkeyI,
    op_ret: Result<(), BchError>,
    flags:  CommitOpts,
) -> Result<(), BchError> {
    if op_ret.as_ref().is_err_and(|e| e.matches(bch_errcode::BCH_ERR_injected_logged_op_fail)) {
        return op_ret;
    }

    commit_do(trans, None, flags.flags(CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC),
              |t| t.delete(c::btree_id::logged_ops, k.k.p, UpdateTriggerFlags::empty()))
    .inspect_err(|e| {
        /*
         * This needs to be a fatal error because we've left an unfinished
         * operation in the logged ops btree.
         *
         * We should only ever see an error here if the filesystem has already
         * been shut down, but make sure of that here:
         */
        let fs = trans.fs();
        fs_fatal_error!(fs, "deleting logged operation {}: {}", k.to_text(fs), e);
    })?;

    op_ret
}

/// # Safety
/// @k is a key.
#[c_export]
pub unsafe extern "C" fn bch2_logged_op_finish(
    trans:        &BtreeTrans<'_>,
    k:            *mut c::bkey_i,
    op_ret:       c_int,
    commit_flags: c_uint,
) -> c_int {
    let k = unsafe { &*BkeyI::from_raw(k) };

    ret_to_c(finish(&trans, k, ret_to_result_void(op_ret), CommitOpts::from_bits(commit_flags)))
}
