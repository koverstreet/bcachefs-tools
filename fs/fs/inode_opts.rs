// SPDX-License-Identifier: GPL-2.0

//! Per-inode io path options: storage, resolution against filesystem defaults,
//! inheritance, propagation to ancestor snapshots.
//!
//! And struct bch_inode_opts, the io options data is written with: the
//! filesystem's, an inode's, and a key's - get_io_opts(), by what the key is,
//! the options of the inode it's in as of its snapshot for a file's data.
//!
//! BCH_INODE_OPTS(): the options an inode can set for itself and what's below
//! it. Stored with a +1 bias: 0 is unset (inherit), 1 is an explicit "none";
//! bi_fields_set says which were set on the inode itself, rather than
//! propagated from a parent. Nothing outside this file should touch
//! bi_<option> directly.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{spos, BkeyI, BkeySC, BkeyWhole, POS_MIN};
use crate::btree::iter::{commit_do, is_restart, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts,
                         TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::data::ptrs::Ptrs;
use crate::data::reconcile::trigger::BkeyVReconcile;
use crate::data::reflink::Indirect;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::snapshots::{snapshot, subvolume};
use crate::util::alloc::{darray_exit, flags, DArrayLoan};
use crate::util::ffi::Opaque;
use crate::util::{Printbuf, ToText};
use crate::{fsck_err_flags, inode, logged_ops, opts};
use core::ffi::{c_int, CStr};
use core::mem::MaybeUninit;
use core::ops::ControlFlow;
use cstruct_macros::c_export;

const NR: usize = c::inode_opt_id::Inode_opt_nr.0 as usize;

struct Desc {
    name: &'static CStr,
    /// The filesystem option it is
    opt:  c::bch_opt_id,
    get:  fn(&c::bch_inode_unpacked) -> u64,
    set:  fn(&mut c::bch_inode_unpacked, u64),
}

const fn c_str(s: &'static str) -> &'static CStr {
    match CStr::from_bytes_with_nul(s.as_bytes()) {
        Ok(s)  => s,
        Err(_) => panic!("not a C string"),
    }
}

macro_rules! descs {
    ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { [$(Desc {
        name: c_str(concat!(stringify!($name), "\0")),
        opt:  c::bch_opt_id::[<Opt_ $name>],
        get:  |inode| inode.[<bi_ $name>] as u64,
        set:  |inode, v| inode.[<bi_ $name>] = v as _,
    }),*] } };
}

/// In BCH_INODE_OPTS() order, which is inode_opt_id's.
const DESCS: [Desc; NR] = c::BCH_INODE_OPTS!(descs);

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
        Self::all().find(|opt| opt.opt_id() == id)
    }

    pub fn id(self) -> c::inode_opt_id {
        self.0
    }

    fn idx(self) -> usize {
        self.0.0 as usize
    }

    fn desc(self) -> &'static Desc {
        &DESCS[self.idx()]
    }

    pub fn opt_id(self) -> c::bch_opt_id {
        self.desc().opt
    }

    pub fn name(self) -> &'static CStr {
        self.desc().name
    }

    /// Its value on @inode, biased: 0 for not set.
    pub fn get(self, inode: &c::bch_inode_unpacked) -> u64 {
        (self.desc().get)(inode)
    }

    /// @v biased, 0 for not set.
    pub fn set(self, inode: &mut c::bch_inode_unpacked, v: u64) {
        (self.desc().set)(inode, v)
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

impl c::bch_inode_opts {
    /// The options that follow from others, and what doesn't combine: as
    /// bch2_io_opts_fixups(), opts.h's - a copy, until opts.c is Rust.
    pub fn fixups(&mut self) {
        /*
         * A derived value comes from wherever its source did: shared data's
         * options keep only what's marked as the inode's, and would lose it.
         */
        if self.background_target == 0 {
            self.background_target = self.foreground_target;
            self.set_background_target_from_inode(self.foreground_target_from_inode());
        }
        if self.background_compression == 0 && !self.background_compression_from_inode() {
            self.background_compression = self.compression;
            self.set_background_compression_from_inode(self.compression_from_inode());
        }
        if self.data_replicas == 1 {
            self.erasure_code = 0;
        }
        if self.nocow != 0 {
            let from_inode = self.nocow_from_inode();
            self.compression = 0;
            self.background_compression = 0;
            self.data_checksum = 0;
            self.erasure_code = 0;
            self.set_compression_from_inode(from_inode);
            self.set_background_compression_from_inode(from_inode);
            self.set_data_checksum_from_inode(from_inode);
            self.set_erasure_code_from_inode(from_inode);
        }
        /* We currently only support up to RAID6: */
        if self.erasure_code != 0 {
            self.data_replicas = self.data_replicas.min(3);
        }
    }
}

/// The filesystem's io options, or for metadata its metadata options:
/// bch2_inode_opts_get().
pub fn get_fs(fs: &Fs, metadata: bool) -> c::bch_inode_opts {
    let mut opts = MaybeUninit::uninit();
    // SAFETY: the filesystem, live as @fs is; @opts written, from a memset()
    unsafe {
        c::bch2_inode_opts_get(fs.as_ptr().cast(), opts.as_mut_ptr(), metadata);
        opts.assume_init()
    }
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

// ── A key's io options ───────────────────────────────────────────────────

/// A walk's per_snapshot_io_opts, Rust's: C's CLASS(per_snapshot_io_opts).
/// Made as per_snapshot_io_opts_init() makes it - the change cookie one
/// behind, so the first get_io_opts() takes the filesystem's - and its
/// darray freed on drop, as _exit() frees it.
pub struct PerSnapshotIoOpts(c::per_snapshot_io_opts);

impl PerSnapshotIoOpts {
    pub fn new(fs: &Fs) -> Self {
        let mut s = c::per_snapshot_io_opts::default();
        s.fs_io_opts.change_cookie = fs.opt_change_cookie.wrapping_sub(1);
        PerSnapshotIoOpts(s)
    }
}

impl core::ops::Deref for PerSnapshotIoOpts {
    type Target = c::per_snapshot_io_opts;
    fn deref(&self) -> &c::per_snapshot_io_opts { &self.0 }
}

impl core::ops::DerefMut for PerSnapshotIoOpts {
    fn deref_mut(&mut self) -> &mut c::per_snapshot_io_opts { &mut self.0 }
}

impl Drop for PerSnapshotIoOpts {
    fn drop(&mut self) {
        // SAFETY: a darray get_io_opts() grew
        unsafe { darray_exit(&mut self.0.d) };
    }
}

/// The io options @k's data is written with: a btree node pointer's, the
/// filesystem's metadata options; a file's data, its inode's, as of its
/// snapshot; reflinked data, the filesystem's, with those its reconcile entry
/// has from an inode; anything else, the filesystem's: bch2_bkey_get_io_opts().
///
/// @s caches them across a scan in key order: the filesystem's, and every
/// snapshot version of the inode it's on.
pub fn get_io_opts(t: &TransAttempt<'_, '_>, s: Option<&mut c::per_snapshot_io_opts>, k: BkeySC<'_>)
    -> Result<c::bch_inode_opts, BchError>
{
    enum Mode { Metadata, Reflink, User }
    let fs = t.fs();

    let mode = if k.k.is_btree_ptr() {
        Mode::Metadata
    } else if k.arm::<Indirect>().is_some() {
        Mode::Reflink
    } else if k.k.is_user_data() || k.k.key_type() == c::bch_bkey_type::KEY_TYPE_reflink_p {
        if k.k.p.snapshot == 0 {
            crate::bch_err!(fs, "user data key with snapshot == 0\n{}", k.to_text(fs));
            crate::warn_on!(true);
            return Ok(get_fs(fs, false));
        }
        Mode::User
    } else {
        // KEY_TYPE_error?
        return Ok(get_fs(fs, false));
    };
    let metadata = matches!(mode, Mode::Metadata);
    let inum = k.k.p.inode;

    let mut opts = match s {
        None => {
            let mut opts = get_fs(fs, metadata);
            if matches!(mode, Mode::User) {
                match inode::find_by_inum_snapshot(t, inum, k.k.p.snapshot, BtreeIterFlags::CACHED) {
                    Ok(inode) => opts = get_inode(fs, &inode),
                    Err(e) if is_restart(&e) => return Err(e),
                    Err(_) => {}
                }
            }
            opts
        }
        Some(s) => {
            if s.fs_io_opts.change_cookie != fs.opt_change_cookie || s.metadata != metadata {
                s.fs_io_opts = get_fs(fs, metadata);
                s.metadata   = metadata;
                s.cur_inum   = 0;
                s.d.nr       = 0;
            }

            if matches!(mode, Mode::User) {
                // SAFETY: C's darray, as darray_push() grows it
                let mut d = unsafe { DArrayLoan::new(&mut s.d) };

                if s.cur_inum != inum {
                    d.clear();

                    // norestart: callers - fsck repair paths - may have
                    // updates queued, which an internal trans_begin() would
                    // clobber
                    let mut iter = BtreeIter::new(t, c::btree_id::inodes, spos(0, inum, 0),
                                                  BtreeIterFlags::ALL_SNAPSHOTS);
                    iter.for_each_max_norestart(t, spos(0, inum, u32::MAX), |_, inode_k| {
                        if inode::bkey_is_inode(inode_k.k) {
                            let inode = inode::unpack(fs, inode_k);
                            let e = c::snapshot_io_opts_entry {
                                snapshot: inode_k.k.p.snapshot,
                                io_opts:  get_inode(fs, &inode),
                            };
                            d.push(e, flags::GFP_KERNEL)?;
                        }
                        Ok(())
                    })?;

                    s.cur_inum = inum;
                    s.inum_scan_cookie = false;
                }

                if let Some(e) = d.iter().find(|e| snapshot::is_ancestor(t, k.k.p.snapshot, e.snapshot)) {
                    return Ok(e.io_opts);
                }
            }
            s.fs_io_opts
        }
    };

    let old = if matches!(mode, Mode::Reflink) {
        k.arm::<Ptrs>().and_then(|k| k.reconcile_opts(fs).copied())
    } else {
        None
    };
    if let Some(old) = old {
        macro_rules! from_inode {
            ($(($name:tt)),* $(,)?) => { crate::paste! { $(
                if old.[<$name _from_inode>]() != 0 {
                    opts.$name = old.$name() as _;
                }
                opts.[<set_ $name _from_inode>](old.[<$name _from_inode>]() != 0);
            )* } };
        }
        c::BCH_RECONCILE_OPTS!(from_inode);

        // The entry was written under the fixups of its day: today's may
        // have grown, and downstream needs a combination valid under them.
        opts.fixups();

        // A checksum or compression type we don't know is an incompat
        // feature, so shouldn't have mounted - this is for a versioning
        // mistake: the filesystem's options, as get_inode() falls back to.
        if opts.data_checksum as u32 >= c::bch_csum_opt::BCH_CSUM_OPT_NR.0 {
            opts.data_checksum = fs.opts.data_checksum as _;
            opts.set_data_checksum_from_inode(false);
        }
        if !c::bch_compression_opt::valid(opts.background_compression as u8) {
            opts.background_compression = fs.opts.background_compression as _;
            opts.set_background_compression_from_inode(false);
        }
    }
    Ok(opts)
}

/// # Safety
/// C has an attempt in progress on @trans; @s's darray is C's.
#[c_export]
pub unsafe fn bch2_bkey_get_io_opts(trans: &BtreeTrans<'_>, s: Option<&mut c::per_snapshot_io_opts>, k: BkeySC<'_>,
                                    opts: &mut MaybeUninit<c::bch_inode_opts>) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(get_io_opts(&t, s, k).map(|o| { opts.write(o); }))
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
            claimed[opt.idx()] = claimed[opt.idx()].max(opt.get(&inode));
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
        let claimed = claimed[opt.idx()];
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
        if t.fs().sb.compat.get() & (1 << c::bch_sb_compat::BCH_COMPAT_inode_opts_propagated.0) == 0 {
            flags |= c::bch_fsck_flags::FSCK_ERR_SILENT;
        }

        let mut buf = Printbuf::new();
        writeln!(buf, "inode options not propagated to ancestor snapshot");
        writeln!(buf, "inum {inum} set at snapshot {origin_snapshot}, ancestor {ancestor} should be:");
        ancestor_inode.to_text(&mut buf);

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
    let op = op_k.arm::<c::LoggedOpInodeOptPropagate>().expect("an inode_opt_propagate op");
    let inum = op.v.inum.get();
    let origin_snapshot = op.v.origin_snapshot.get();

    while let Some(ancestor) = fs.snapshots.parent(op.v.cursor_snapshot.get()) {
        /* advanced in the same commit as the work it describes */
        op.v.cursor_snapshot = ancestor.into();

        let mut done = false;
        commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| {
            done = propagate_one(t, inum, origin_snapshot, ancestor, None)?.is_break();
            logged_ops::update(t, op.as_ref())
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

    logged_ops::start(t, op.as_mut())
}

/* After that transaction has committed: climb, then drop the op */
fn propagate_finish(trans: &BtreeTrans<'_>, op: &mut c::bkey_i_logged_op_inode_opt_propagate)
    -> Result<(), BchError>
{
    let ret = resume_propagate(trans, op.as_mut());
    logged_ops::finish(trans, op.as_ref(), ret, CommitOpts::new())
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
    trans:    &BtreeTrans<'_>,
    old_r:    &c::bch_extent_reconcile,
    inode_u:  &c::bch_inode_unpacked,
    snapshot: u32,
    ch:       &mut c::inode_opt_change,
) -> c_int {
    ret_to_c(ch.record(&unsafe { trans.attempt_in_progress() }, old_r, inode_u, snapshot))
}

#[c_export]
pub fn bch2_inode_opt_change_finish(trans: &BtreeTrans<'_>, ch: &mut c::inode_opt_change) -> c_int {
    ret_to_c(ch.finish(trans))
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
