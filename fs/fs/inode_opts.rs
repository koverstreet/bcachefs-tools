// SPDX-License-Identifier: GPL-2.0

//! Per-inode io path options: storage, resolution against filesystem defaults,
//! inheritance, propagation to ancestor snapshots.
//!
//! BCH_INODE_OPTS(): the options an inode can set for itself and what's below
//! it. Stored with a +1 bias: 0 is unset (inherit), 1 is an explicit "none";
//! bi_fields_set says which were set on the inode itself, rather than
//! propagated from a parent. Nothing outside this file should touch
//! bi_<option> directly.

use crate::btree::bkey::{spos, BkeyI, BkeySC, SizedBkey, POS_MIN};
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts,
                         TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::snapshots::{snapshot, subvolume};
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use crate::{fsck_err_flags, inode, logged_ops, opts};
use core::ffi::{c_int, CStr};
use core::ops::ControlFlow;
use cstruct_macros::c_export;

const NR: usize = c::inode_opt_id::Inode_opt_nr.0 as usize;

/// An inode option: an inode_opt_id below Inode_opt_nr.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct InodeOpt(c::inode_opt_id);

impl InodeOpt {
    pub fn all() -> impl Iterator<Item = InodeOpt> {
        (0..c::inode_opt_id::Inode_opt_nr.0).map(|i| InodeOpt(c::inode_opt_id(i)))
    }

    /// The inode option filesystem option @id is, if it's one: as
    /// bch2_opt_to_inode_opt().
    pub fn from_opt_id(id: c::bch_opt_id) -> Option<InodeOpt> {
        macro_rules! from_opt_id {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
                if id == c::bch_opt_id::[<Opt_ $name>] {
                    return Some(InodeOpt(c::inode_opt_id::[<Inode_opt_ $name>]));
                }
            )* } };
        }
        c::BCH_INODE_OPTS!(from_opt_id);
        None
    }

    pub fn id(self) -> c::inode_opt_id {
        self.0
    }

    /// The filesystem option it is.
    pub fn opt_id(self) -> c::bch_opt_id {
        macro_rules! opt_id {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
                if self.0 == c::inode_opt_id::[<Inode_opt_ $name>] {
                    return c::bch_opt_id::[<Opt_ $name>];
                }
            )* } };
        }
        c::BCH_INODE_OPTS!(opt_id);
        panic!("inode option {} out of range", self.0.0)
    }

    pub fn name(self) -> &'static CStr {
        macro_rules! names {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { [$(
                match CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()) {
                    Ok(s)  => s,
                    Err(_) => panic!("an option name with a NUL in it"),
                }
            ),*] };
        }
        const NAMES: [&CStr; NR] = c::BCH_INODE_OPTS!(names);
        NAMES[self.0.0 as usize]
    }

    /// Its value on @inode, biased: 0 for not set.
    pub fn get(self, inode: &c::bch_inode_unpacked) -> u64 {
        macro_rules! get {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
                if self.0 == c::inode_opt_id::[<Inode_opt_ $name>] {
                    return inode.[<bi_ $name>] as u64;
                }
            )* } };
        }
        c::BCH_INODE_OPTS!(get);
        panic!("inode option {} out of range", self.0.0)
    }

    /// @v biased, 0 for not set.
    pub fn set(self, inode: &mut c::bch_inode_unpacked, v: u64) {
        macro_rules! set {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
                if self.0 == c::inode_opt_id::[<Inode_opt_ $name>] {
                    inode.[<bi_ $name>] = v as _;
                    return;
                }
            )* } };
        }
        c::BCH_INODE_OPTS!(set);
        panic!("inode option {} out of range", self.0.0)
    }

    /// Whether it's set on @inode itself, not inherited: bi_fields_set.
    pub fn is_own(self, inode: &c::bch_inode_unpacked) -> bool {
        inode.bi_fields_set & (1 << self.0.0) != 0
    }

    pub fn set_own(self, inode: &mut c::bch_inode_unpacked, own: bool) {
        let bit = 1 << self.0.0;
        if own {
            inode.bi_fields_set |= bit;
        } else {
            inode.bi_fields_set &= !bit;
        }
    }
}

/// Copy the options @dst inherits from @src - everything it hasn't set for
/// itself, and casefolding only for a directory. Called when a file moves
/// between directories. Whether anything changed.
pub fn reinherit_attrs(dst: &mut c::bch_inode_unpacked, src: &c::bch_inode_unpacked) -> bool {
    let mut ret = false;

    for opt in InodeOpt::all() {
        if !dst.is_dir() && opt.id() == c::inode_opt_id::Inode_opt_casefold {
            continue;
        }

        if opt.is_own(dst) {
            continue;
        }

        let v = opt.get(src);
        if v == opt.get(dst) {
            continue;
        }

        opt.set(dst, v);
        ret = true;
    }

    ret
}

#[c_export]
pub fn bch2_reinherit_attrs(dst_u: &mut c::bch_inode_unpacked, src_u: &c::bch_inode_unpacked) -> bool {
    reinherit_attrs(dst_u, src_u)
}

/// Those @inode sets, with the +1 bias removed.
pub fn inode_opts_to_opts(inode: &c::bch_inode_unpacked) -> c::bch_opts {
    let mut ret = c::bch_opts::default();

    for opt in InodeOpt::all() {
        let v = opt.get(inode);
        if v != 0 {
            opts::opt_set_by_id(&mut ret, opt.opt_id(), v - 1);
        }
    }

    ret
}

/// @inode's io options: its own where it sets them, the filesystem's where it
/// doesn't, and for each which - as bch2_inode_opts_get_inode().
pub fn get_inode(fs: &Fs, inode: &c::bch_inode_unpacked) -> c::bch_inode_opts {
    let mut ret = c::bch_inode_opts::default();

    macro_rules! get {
        ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
            if inode.[<bi_ $name>] != 0 {
                ret.$name = (inode.[<bi_ $name>] - 1) as _;
                ret.[<set_ $name _from_inode>](true);
            } else {
                ret.$name = fs.opts.$name as _;
                ret.[<set_ $name _from_inode>](false);
            }
        )* } };
    }
    c::BCH_INODE_OPTS!(get);

    /*
     * Forward compatibility: inodes written by newer versions may carry
     * checksum/compression types we don't know about — fall back to the
     * filesystem option for new writes. Reads are unaffected, extents
     * carry their own types. (This is why these aren't validated at
     * btree read time: that would reject valid inodes from newer
     * versions.)
     */
    if ret.data_checksum as u32 >= c::bch_csum_opt::BCH_CSUM_OPT_NR.0 {
        ret.data_checksum = fs.opts.data_checksum as _;
        ret.set_data_checksum_from_inode(false);
    }
    if !c::bch_compression_opt::valid(ret.compression as u8) {
        ret.compression = fs.opts.compression as _;
        ret.set_compression_from_inode(false);
    }
    if !c::bch_compression_opt::valid(ret.background_compression as u8) {
        ret.background_compression = fs.opts.background_compression as _;
        ret.set_background_compression_from_inode(false);
    }

    ret.change_cookie = fs.opt_change_cookie;

    ret.fixups();
    ret
}

#[c_export]
pub fn bch2_inode_opts_get_inode(
    c:     &Opaque<c::bch_fs>,
    inode: &c::bch_inode_unpacked,
    ret:   &mut c::bch_inode_opts,
) {
    *ret = get_inode(Fs::from_c(c), inode);
}

// ── Propagation to ancestor snapshots ────────────────────────────────────

/*
 * Per option, the strongest value held by an inode version below @ancestor and
 * off the path from @origin_snapshot; 0 if none.
 *
 * A snapshot with no inode key isn't seen, deliberately: it reads @ancestor,
 * so it should follow the change.
 */
fn claimed_off_path(t: &TransAttempt<'_, '_>, inum: u64, ancestor: u32, origin_snapshot: u32)
    -> Result<[u64; NR], BchError>
{
    let fs = t.fs();
    let mut claimed = [0; NR];

    let mut iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, inum, 0), BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_max_norestart(t, spos(0, inum, u32::MAX), |_, k| {
        let snapshot = k.k.p.snapshot;

        if !inode::bkey_is_inode(k.k) ||
           snapshot == ancestor ||
           !snapshot::is_ancestor(t, snapshot, ancestor) ||
           snapshot::is_ancestor(t, origin_snapshot, snapshot) {
            return Ok(());
        }

        let inode = inode::unpack(fs, k);

        for opt in InodeOpt::all() {
            let c = &mut claimed[opt.id().0 as usize];
            *c = (*c).max(opt.get(&inode));
        }
        Ok(())
    })?;

    Ok(claimed)
}

/*
 * Is @snapshot the master subvolume - the one in a tree that isn't itself a
 * snapshot? check_snapshot_tree() maintains that.
 *
 * A tree need not have one: set_deleted() clears master_subvol and only fsck
 * elects a replacement. Not damage, just no tiebreak - hence ENOENT is false,
 * not an error.
 */
fn snapshot_is_master_subvol(t: &TransAttempt<'_, '_>, snapshot: u32) -> Result<bool, BchError> {
    let st = match snapshot::tree_lookup(t, t.fs().snapshots.tree(snapshot)) {
        Err(e) if e.matches(c::ENOENT) => return Ok(false),
        r => r?,
    };

    let master_subvol = st.master_subvol.get();
    if master_subvol == 0 {
        return Ok(false);
    }

    match subvolume::get(t, master_subvol, false) {
        Err(e) if e.matches(c::ENOENT) => Ok(false),
        r => Ok(r?.snapshot.get() == snapshot),
    }
}

/*
 * Which branch wins: see the Principles of Operation.
 *
 * data_replicas maxes over @src and claims, not @dst - that would ratchet.
 * @dst agreeing skips the write, not the climb.
 */
fn merge_into_ancestor(
    src:           &c::bch_inode_unpacked,
    dst:           &mut c::bch_inode_unpacked,
    claimed:       &[u64; NR],
    src_is_master: bool,
) -> bool {
    let mut changed = false;

    for opt in InodeOpt::all() {
        let claimed = claimed[opt.id().0 as usize];
        let mut v = opt.get(src);
        if v == 0 {
            continue;
        }

        if opt.id() == c::inode_opt_id::Inode_opt_data_replicas {
            v = v.max(claimed);
        } else if claimed != 0 && !src_is_master {
            continue;
        }

        if opt.get(dst) == v {
            continue;
        }

        opt.set(dst, v);
        changed = true;
    }

    changed
}

/// The inode at @iter's position, if there's one.
fn peek_inode<'t>(t: &TransAttempt<'_, 't>, iter: &mut BtreeIter<'t>)
    -> Result<Option<c::bch_inode_unpacked>, BchError>
{
    let k = iter.peek_slot(t)?.expect("a slot always has a key");
    Ok(inode::bkey_is_inode(k.k).then(|| inode::unpack(t.fs(), k)))
}

/*
 * Queue what @ancestor should take from the version at @origin_snapshot.
 *
 * Break means the whole op is moot - the source inode was unlinked - not that
 * this level had nothing to do.
 *
 * @fsck_repaired reports before writing, and says whether we did; the check
 * is then the same code as the rule.
 */
fn propagate_one(
    t:               &TransAttempt<'_, '_>,
    inum:            u64,
    origin_snapshot: u32,
    ancestor:        u32,
    fsck_repaired:   Option<&mut bool>,
) -> Result<ControlFlow<()>, BchError> {
    let mut src_iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, inum, origin_snapshot),
                                      BtreeIterFlags::ALL_SNAPSHOTS);
    let Some(src) = peek_inode(t, &mut src_iter)? else {
        return Ok(ControlFlow::Break(()));
    };

    let mut iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, inum, ancestor),
                                  BtreeIterFlags::INTENT | BtreeIterFlags::ALL_SNAPSHOTS);
    let Some(mut ancestor_inode) = peek_inode(t, &mut iter)? else {
        return Ok(ControlFlow::Continue(()));
    };

    let claimed = claimed_off_path(t, inum, ancestor, origin_snapshot)?;
    let src_is_master = snapshot_is_master_subvol(t, origin_snapshot)?;

    if !merge_into_ancestor(&src, &mut ancestor_inode, &claimed, src_is_master) {
        return Ok(ControlFlow::Continue(()));
    }

    if let Some(repaired) = fsck_repaired {
        /*
         * Repair quietly until the compat bit says a full check has
         * run: before that the filesystem predates propagation, so
         * finding work is the upgrade, not a bug.
         */
        let mut flags = c::bch_fsck_flags::FSCK_CAN_FIX | c::bch_fsck_flags::FSCK_CAN_IGNORE;
        if t.fs().sb.compat & (1 << c::bch_sb_compat::BCH_COMPAT_inode_opts_propagated.0) == 0 {
            flags |= c::bch_fsck_flags::FSCK_ERR_SILENT;
        }

        let mut buf = Printbuf::new();
        writeln!(buf, "inode options not propagated to ancestor snapshot");
        writeln!(buf, "inum {inum} set at snapshot {origin_snapshot}, ancestor {ancestor} should be:");
        inode::unpacked_to_text(&mut buf, &ancestor_inode);

        if !fsck_err_flags!(t, POS_MIN, flags, id::inode_opts_not_propagated, "{}", buf.as_str())? {
            return Ok(ControlFlow::Continue(()));
        }

        *repaired = true;
    }

    /* in place: the normal update path would COW it down into a leaf */
    inode::write_flags(t, &mut iter, &mut ancestor_inode, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    Ok(ControlFlow::Continue(()))
}

#[cold]
#[c_export]
pub fn bch2_logged_op_inode_opt_propagate_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let op = c::LoggedOpInodeOptPropagate::val_copy_pad(k).expect("an inode_opt_propagate op");

    write!(out, "inum={} origin={} cursor={}",
           op.inum.get(), op.origin_snapshot.get(), op.cursor_snapshot.get());
}

/// A reconcile scan of @inum's data, for options it may have that its data
/// doesn't.
fn needs_scan(t: &TransAttempt<'_, '_>, inum: u64) -> Result<(), BchError> {
    let scan = c::reconcile_scan {
        type_: c::reconcile_scan_type::RECONCILE_SCAN_inum,
        id:    c::reconcile_scan_id { inum },
    };

    t.result(unsafe { c::bch2_set_reconcile_needs_scan_trans(t.raw(), scan) })
}

/// Climb from the op's cursor to the root, a commit per level: as
/// bch2_resume_logged_op_inode_opt_propagate().
pub fn resume_propagate(trans: &BtreeTrans<'_>, op_k: &mut BkeyI) -> Result<(), BchError> {
    let fs = trans.fs();
    let op = op_k.arm_mut::<c::LoggedOpInodeOptPropagate>().expect("an inode_opt_propagate op");
    let inum = op.v.inum.get();
    let origin_snapshot = op.v.origin_snapshot.get();

    while let Some(ancestor) = fs.snapshots.parent(op.v.cursor_snapshot.get()) {
        /* advanced in the same commit as the work it describes */
        op.v.cursor_snapshot = ancestor.into();

        let mut done = false;
        commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| {
            done = propagate_one(t, inum, origin_snapshot, ancestor, None)?.is_break();
            logged_ops::update(t, op.k_i())
        })?;
        if done {
            break;
        }
    }

    /*
     * The "after" half of the opt change bracket, which for an inode option
     * lands here, not when setxattr returns: a scan before the ancestors
     * carry the new options would find nothing to do.
     */
    commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| needs_scan(t, inum))?;
    unsafe { c::rust_bch2_reconcile_wakeup(fs.as_ptr()) };
    Ok(())
}

/*
 * Goes in the caller's transaction, so the op commits with the change that
 * needs it: a crash can't leave the option set with no climb recorded.
 *
 * @op is left deleted if there's nothing above @snapshot; the caller runs
 * propagate_finish() only if it was armed.
 */
fn propagate_start(
    t:        &TransAttempt<'_, '_>,
    inum:     u64,
    snapshot: u32,
    op:       &mut c::bkey_i_logged_op_inode_opt_propagate,
) -> Result<(), BchError> {
    op.k = c::bkey::new();

    if t.fs().snapshots.parent(snapshot).is_none() {
        return Ok(());
    }

    *op = BkeyI::new();
    op.v.inum            = inum.into();
    op.v.origin_snapshot = snapshot.into();
    op.v.cursor_snapshot = snapshot.into();

    logged_ops::start(t, op.k_i_mut())
}

/* After that transaction has committed: climb, then drop the op */
fn propagate_finish(trans: &BtreeTrans<'_>, op: &mut c::bkey_i_logged_op_inode_opt_propagate)
    -> Result<(), BchError>
{
    let ret = resume_propagate(trans, op.k_i_mut());
    logged_ops::finish(trans, op.k_i(), ret, CommitOpts::new())
}

impl c::inode_opt_change {
    /// At the start of the transaction: a restart discards the logged op.
    pub fn new() -> Self {
        let mut ch = Self::default();
        ch.propagate.k = c::bkey::new();
        ch
    }

    /// In the transaction, after changing @inode's options: @old is its
    /// reconcile opts from before, @snapshot the snapshot the inode is
    /// written at. If they changed, schedule the scan and start the
    /// propagate.
    pub fn record(
        &mut self,
        t:        &TransAttempt<'_, '_>,
        old:      &c::bch_extent_reconcile,
        inode:    &c::bch_inode_unpacked,
        snapshot: u32,
    ) -> Result<(), BchError> {
        let new = inode::reconcile_opts_get(t.fs(), inode)?;

        self.reconcile_changed = old.into_bits() != new.into_bits();
        if !self.reconcile_changed {
            return Ok(());
        }

        needs_scan(t, inode.bi_inum)?;
        /*
         * Data written before this subvolume branched off is keyed at an
         * ancestor snapshot, and takes its options from the inode version
         * there: the new options have to be pushed up to reach it.
         */
        propagate_start(t, inode.bi_inum, snapshot, &mut self.propagate)
    }

    /// After the transaction has committed: finish the propagate, if one was
    /// started, and wake reconcile, if there's a scan for it.
    pub fn finish(&mut self, trans: &BtreeTrans<'_>) -> Result<(), BchError> {
        let ret = if self.propagate.k.is::<c::LoggedOpInodeOptPropagate>() {
            propagate_finish(trans, &mut self.propagate)
        } else {
            Ok(())
        };

        if self.reconcile_changed {
            unsafe { c::rust_bch2_reconcile_wakeup(trans.fs().as_ptr()) };
        }
        ret
    }
}

#[c_export]
pub fn bch2_inode_opt_change_init(ch: &mut c::inode_opt_change) {
    *ch = c::inode_opt_change::new();
}

#[c_export]
pub fn bch2_inode_opt_change_trans(
    trans:    &Opaque<c::btree_trans>,
    old_r:    &c::bch_extent_reconcile,
    inode_u:  &c::bch_inode_unpacked,
    snapshot: u32,
    ch:       &mut c::inode_opt_change,
) -> c_int {
    let trans = BtreeTrans::from_c(trans);
    ret_to_c(ch.record(&unsafe { trans.attempt_in_progress() }, old_r, inode_u, snapshot))
}

#[c_export]
pub fn bch2_inode_opt_change_finish(trans: &Opaque<c::btree_trans>, ch: &mut c::inode_opt_change) -> c_int {
    ret_to_c(ch.finish(&BtreeTrans::from_c(trans)))
}

/// Check that @inode's options have been pushed up to its ancestor snapshots,
/// repairing if not: as bch2_check_inode_opts_propagated().
///
/// An invariant, not a one-off migration: deleting one of two disagreeing
/// branches leaves the survivor's value uncontested but never propagated, and
/// nothing re-runs the climb on snapshot deletion.
///
/// No logged op - fsck reruns from the start anyway, and a begin in its climb
/// would invalidate check_inodes()'s iterator.
pub fn check_propagated(t: &TransAttempt<'_, '_>, inode: &c::bch_inode_unpacked) -> Result<(), BchError> {
    if !inode.flag(c::bch_inode_flags::BCH_INODE_has_inode_opts) {
        return Ok(());
    }

    let mut repaired = false;
    let mut ancestor = inode.bi_snapshot;
    while let Some(a) = t.fs().snapshots.parent(ancestor) {
        ancestor = a;
        if propagate_one(t, inode.bi_inum, inode.bi_snapshot, ancestor, Some(&mut repaired))?.is_break() {
            break;
        }
    }

    /*
     * We've written keys outside the one check_inodes() handed us, at
     * snapshots it isn't iterating; commit them and restart so it resumes
     * from a consistent position.
     */
    if repaired {
        t.commit(None, CommitFlags::NO_ENOSPC)?;
        return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested));
    }

    Ok(())
}
