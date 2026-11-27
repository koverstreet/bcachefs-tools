// SPDX-License-Identifier: GPL-2.0

//! The VFS's inode, InodeInfo - C's struct bch_inode_info - for Rust's VFS
//! entry points. One borrowed for a VFS call is a &InodeInfo: the call holds
//! a reference on the inode for as long as it runs. InodeRef is one Rust holds
//! a reference on itself.
//!
//! ei_inode, the inode as last written, is read without a lock: its writers
//! are serialized by the inode's btree position, which a transaction still
//! holds locked after its commit, through update_after_write().
//!
//! Its struct inode is @v, a VfsInode (vfs_inode.rs): what Rust does of the
//! VFS inode, it does through that.
//!
//! ei_update_lock, UpdateLock, serializes what the btree doesn't: a quota
//! transfer and the inode write that goes with it - set_projid(), setattr's,
//! rename's and the reinherit ioctl's - so the inode and its quota agree.
//!
//! ei_quota_lock, ei_quota, serializes the inode's quota accounting -
//! reservations and i_sectors_acct, against a transfer. ei_qid changes only
//! with both held, so either is enough to read it.
//!
//! The times go both ways: the btree's writes set them, and
//! update_after_write() copies them in; the VFS sets them itself - atime on
//! access, mtime and ctime on a data write - and ->write_inode writes them
//! back. Every other field is the btree's.
//!
//! The VFS's updates reach the btree only at writeback - with lazytime, as
//! late as sync or eviction - so an inode write made meanwhile has to carry
//! them, or a crash can keep it and lose them (generic/622): fold_times(),
//! before the write's own changes, which still win - assigned, not max()ed,
//! as times can be set backwards. posix/namei.rs's writes peek the inode
//! themselves: fold_times_trans() writes the folded times first, in the same
//! transaction.

#![allow(non_camel_case_types)]

use crate::data_structures::{array_vec::ArrayVec, bitmap::AtomicBitflags};
use core::ffi::CStr;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic::{fence, Ordering::SeqCst};

use crate::btree::bkey::pos;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt};
use crate::c;
use crate::init::{dev::AtomicDevsMask, error::{count_fsck_err, id}};
use crate::posix::inode::{InodeUnpacked, VolatileInodeUnpacked};
use crate::posix::inode_format::InodeOptId;
use crate::posix::quota_format::QuotaType;
use crate::snapshots::{format::Subvolume, subvolume::SubvolInum, subvolume};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::posix::{inode, quota};
use crate::posix::inode_opts::{InodeOpt, InodeOptChange};
use crate::locking::{ExclusiveGuard, Locked, Mutex, SpinLock, SpinLockIrq, TwoStateLock,
    TwoState, TwoStateGuard};
use crate::{published, prt};
use crate::posix::quota::{QuotaAcctMode, QuotaWarnings, Qid};
use crate::util::{ffi::Opaque, log::LogMsg, projections::Volatile, rcu::{read_lock, ReadGuard}};
use crate::os::iattr::{ATTR_ATIME, ATTR_CTIME, ATTR_MTIME, ATTR_SIZE};
use crate::os::kernel::HZ;
use crate::os::workqueue::RawDelayedWork;
use cstruct_macros::{bch_enum, MaybeZeroable, TypeInfo};
use kernel::sync::aref::ARef;
use pin_init::pin_data;

use super::new_inode::NewInode;
use crate::os::{super_block::IsInode, vfs_inode::{ILocked, Live, VfsInode}};

use crate::format::Btree;

/*
 * The VFS's inode, and what we keep beside it. Shared - the kernel and C use it
 * while Rust has a &InodeInfo - so what Rust touches is typed for that: @v,
 * the struct inode the kernel writes, a VfsInode; ei_inode a
 * VolatileInodeUnpacked; ei_update_lock and ei_quota_lock Locked, the second
 * over what it protects. St is @v's state: New while it's a NewInode's.
 */
#[pin_data]
#[repr(C)]
#[derive(TypeInfo)]
pub struct InodeInfo<St = Live> {
    #[pin]
    pub v: VfsInode<crate::fs::Fs, St>,
    /* the inode cache's key: set before it's in, unchanged while it is */
    pub ei_inum: SubvolInum,

    #[pin]
    pub ei_reserved: Locked<SpinLockIrq, InodeReserved, InodeReservedRange>,

    pub ei_inodes_idx: u32,
    pub ei_flags: AtomicBitflags<InodeInfoFlag>,

    #[pin]
    pub ei_update_lock: Locked<Mutex>,
    pub ei_last_dirtied: Volatile<usize>,
    #[pin]
    pub ei_pagecache_lock: TwoStateLock,

    /*
     * The quota the inode's usage is charged to: i_blocks + ei_quota_reserved
     * is charged to ei_qid. i_blocks, the VFS inode's, changes under the lock
     * too. ei_qid changes with ei_update_lock held as well, so C reads it
     * under either.
     */
    #[pin]
    pub ei_quota: Locked<SpinLock, InodeQuota>,

    /*
     * When we've been doing nocow writes we'll need to issue flushes to the
     * underlying block devices
     *
     * XXX: a device may have had a flush issued by some other codepath. It
     * would be better to keep for each device a sequence number that's
     * incremented when we isusue a cache flush, and track here the sequence
     * number that needs flushing.
     */
    pub ei_devs_need_flush: AtomicDevsMask,

    /*
     * copy of inode in btree: written after a commit, the inode's btree
     * position still locked - which serializes its writers, not
     * ei_update_lock - and read without a lock
     */
    pub ei_inode: VolatileInodeUnpacked,

    #[pin]
    pub ei_writeback_timer: RawDelayedWork,
}

#[bch_enum(closed)]
#[repr(u32)]
pub enum InodeInfoFlag {
    /*
     * A btree error on this inode: the VFS inode and the btree's may
     * disagree
     */
    EI_INODE_ERROR = 0,
    /* in a snapshot subvolume, which quotas don't count */
    EI_INODE_SNAPSHOT = 1,
    /*
     * set by a truncate of a nonempty file to 0: start writeback on
     * close - see DOC(replace-in-place) in vfs/file.rs
     */
    EI_INODE_FLUSH_ON_CLOSE = 2,
}

/*
 * Cached extent allocation state for [start, end), for skipping btree
 * lookups when initializing bch_folio state in sequential buffered
 * writes.
 *
 * Staleness contract: extents may change toward allocated without
 * notice (worst case we over-reserve); anything that deallocates must
 * clear the range, under pagecache_block so no fill (under
 * pagecache_add) can straddle the deallocation. Fillers build their
 * result in locals and publish it in one go: fillers aren't
 * serialized against each other (page_mkwrite fills without i_rwsem),
 * and incremental publishing would let two fills interleave into a
 * range neither of them scanned.
 */
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct InodeReserved {
    pub ei_reserved_replicas: u8,
    pub ei_reserved_state: u8,
}

published! {
    /// InodeReserved's range: written under ei_reserved_lock, and read without
    /// it by the buffered write path, to take the lock only for a folio in it.
    #[derive(MaybeZeroable)]
    pub struct InodeReservedRange {
        pub ei_reserved_start: Volatile<u64>,
        pub ei_reserved_end: Volatile<u64>,
    }
}

/// An inode's quota accounting, under ei_quota_lock.
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct InodeQuota {
    pub ei_qid: Qid,
    /// sectors reserved for writes, charged to ei_qid ahead of i_blocks
    pub ei_quota_reserved: u64,
}

/// ei_pagecache_lock's states: adding to the page cache, or blocking adds.
pub enum PagecacheAdd {}
pub enum PagecacheBlock {}

impl TwoState for PagecacheAdd {
    const S: bool = false;
}

impl TwoState for PagecacheBlock {
    const S: bool = true;
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct QuotaRes {
    pub sectors: u64,
}

pub type InodeRef = ARef<InodeInfo>;

// to_bch_ei(): an InodeInfo is where its struct inode is
crate::is_inode!(InodeInfo { v }: Fs, super::new_inode::INODE_CACHE);

impl InodeInfo {
    /// # Safety
    /// @vinode is one of ours, live for 'a: the VFS holds a reference on it,
    /// or otherwise keeps it from eviction.
    pub unsafe fn from_inode<'a>(vinode: *mut c::inode) -> &'a InodeInfo {
        // SAFETY: the caller's; @v is first
        unsafe { &*vinode.cast::<InodeInfo>() }
    }

    /// @inum's, looked up in @t, the inode cache first.
    pub fn get_trans(t: &TransAttempt<'_, '_>, inum: SubvolInum) -> Result<InodeRef, BchError> {
        Self::get_in(t, inum, Some(crate::here!()))
    }

    /// get_trans(), logging as @warn if it isn't there.
    fn get_in(t: &TransAttempt<'_, '_>, inum: SubvolInum, warn: Option<&CStr>) -> Result<InodeRef, BchError> {
        if let Some(inode) = t.fs().vfs.inode_cache.find(Some(t), inum)? {
            return Ok(inode);
        }
        let subvol = subvolume::get(t, inum.subvol as u32, warn.is_some())?;
        let bi = inode::find_by_inum_in_snapshot(t, inum, subvol.snapshot.get(), warn)?;
        Self::cache_insert(t, inum, &bi, &subvol)
    }

    /// @inum's, the inode cache first, in a transaction of its own - and no
    /// warning if it isn't there, as a file handle's may not be.
    pub(crate) fn get(fs: &Fs, inum: SubvolInum) -> Result<InodeRef, BchError> {
        Self::get_outside(fs, inum, None)
    }

    /// As get(), warning as @func if it isn't there: one that has to be, the
    /// root at mount.
    pub(crate) fn get_warn(fs: &Fs, inum: SubvolInum, func: &CStr) -> Result<InodeRef, BchError> {
        Self::get_outside(fs, inum, Some(func))
    }

    /// The cache, then a transaction.
    fn get_outside(fs: &Fs, inum: SubvolInum, warn: Option<&CStr>) -> Result<InodeRef, BchError> {
        if let Some(inode) = fs.vfs.inode_cache.find(None, inum)? {
            return Ok(inode);
        }
        // before the locks its insert will be under
        fs.vfs.inode_cache.make_room();
        let trans = crate::btree_trans!(fs);
        trans.lockrestart_do(|t| Self::get_in(t, inum, warn))
    }

    /// A new VFS inode for @inum, made from @bi and put in the inode cache -
    /// or the one already there, if another lookup got there first. No
    /// restart is taken from here on but in allocating the inode, and nothing
    /// after: commit what the caller has first.
    pub(crate) fn cache_insert(t: &TransAttempt<'_, '_>, inum: SubvolInum,
                               bi: &InodeUnpacked, subvol: &Subvolume)
        -> Result<InodeRef, BchError>
    {
        let mut inode = NewInode::new_trans(t)?;
        inode.init(t.trans(), inum, bi, subvol);
        t.fs().vfs.inode_cache.insert(t, inode)
    }

    /// The VFS done with it: deleted, if it has no links left.
    pub(super) fn evict(&self) {
        let fs = self.fs();
        let v = self.vfs();
        // An unlinked subvolume's root inode reads i_nlink == 0, but its
        // deletion belongs to the subvolume deletion path, not us (see
        // is_subvolume_root()): this eviction completing is what the
        // subvolume delete work (SubvolDelete, subvolume.rs) is waiting on to
        // tombstone the subvolume and hand the keys to the snapshot sweep.
        let delete = v.nlink() == 0 && !v.is_bad() && !self.ei_inode.get().is_subvolume_root();

        // evict() has waited for outstanding writeback, we'll do no more IO
        // through this inode: it's safe to remove from VFS inode hashtable here
        //
        // Do that now so that other threads aren't blocked from pulling it back
        // in, there's no reason for them to be:
        if !delete {
            fs.vfs.inode_cache.remove(self);
        }

        if cfg!(CONFIG_BCACHEFS_DEBUG) && delete {
            let _ = v.write_now(true);
        }
        v.truncate_pages_final();

        self.ei_writeback_timer.cancel_sync();

        v.clear();

        let (qid, reserved) = {
            let q = self.ei_quota.lock();
            (q.ei_qid, q.ei_quota_reserved)
        };
        assert!(v.is_bad() || reserved == 0, "evicting inode {}:{} with {reserved} sectors of quota reserved",
                self.ei_inum.subvol, self.ei_inum.inum);

        if delete {
            if !self.ei_flags.get(InodeInfoFlag::EI_INODE_SNAPSHOT) {
                let w = &mut QuotaWarnings::new(fs);
                let _ = quota::acct(fs, qid, quota::Q_SPC, -(v.blocks() as i64), QuotaAcctMode::WARN, w);
                let _ = quota::acct(fs, qid, quota::Q_INO, -1, QuotaAcctMode::WARN, w);
            }
            let _ = inode::rm(fs, self.inum());

            // If we are deleting, we need it present in the vfs hash table
            // so that fsck can check if unlinked inodes are still open:
            fs.vfs.inode_cache.remove(self);
        }

        fs.vfs.inodes.remove(self.ei_inodes_idx);
        // evict_subvolume_inodes() checks the slot once it's on the waitqueue:
        // the slot is clear before the VFS's wake, after we return, looks for
        // waiters - with no barrier of its own
        fence(SeqCst);
    }

    pub fn raw(&self) -> *mut InodeInfo {
        core::ptr::from_ref(self).cast_mut()
    }
}

// a new inode's too, NewInode::init()'s
impl<St> InodeInfo<St> {
    pub fn fs(&self) -> &Fs {
        self.v.sb().fs()
    }

    pub fn vfs(&self) -> &VfsInode<Fs, St> {
        &self.v
    }

    pub fn inum(&self) -> SubvolInum {
        self.ei_inum
    }

    /// A copy of ei_inode.
    pub fn inode(&self) -> InodeUnpacked {
        self.ei_inode.get()
    }

    /// Update the VFS inode to @bi, just committed in @trans - which still
    /// holds the inode's btree position locked. @fields: the ATTR_* that
    /// changed.
    pub fn update_after_write(&self, trans: &BtreeTrans<'_>, bi: &InodeUnpacked, fields: u32) {
        assert_eq!(bi.bi_inum, self.inum().inum, "updating the VFS inode {} to inode {}",
                   self.inum().inum, bi.bi_inum);
        trans.assert_pos_locked(Btree::inodes, pos(0, bi.bi_inum));

        let fs = self.fs();
        let v = self.vfs();
        v.set_nlink(bi.nlink());
        v.set_uid(bi.bi_uid);
        v.set_gid(bi.bi_gid);
        v.set_mode(bi.bi_mode);

        if fields & ATTR_SIZE != 0 {
            v.set_size(bi.bi_size);
        }
        if fields & ATTR_ATIME != 0 {
            v.set_atime(fs.time_to_timespec(bi.bi_atime as i64));
        }
        if fields & ATTR_MTIME != 0 {
            v.set_mtime(fs.time_to_timespec(bi.bi_mtime as i64));
        }
        if fields & ATTR_CTIME != 0 {
            v.set_ctime(fs.time_to_timespec(bi.bi_ctime as i64));
        }

        use crate::posix::inode_format::InodeFlags as F;
        for (on, flag) in [(bi.flag(F::sync),      c::S_SYNC),
                           (bi.flag(F::immutable), c::S_IMMUTABLE),
                           (bi.flag(F::append),    c::S_APPEND),
                           (bi.flag(F::noatime),   c::S_NOATIME),
                           (bi.casefold(fs),                 c::S_CASEFOLD)] {
            v.set_flag(flag, on);
        }

        self.ei_inode.set(bi);
    }
}

impl InodeInfo {
    /// @bi, about to be written, with the VFS's times: see the file comment.
    pub(crate) fn fold_times(&self, bi: &mut InodeUnpacked) {
        let (fs, v) = (self.fs(), self.vfs());
        bi.bi_atime = fs.timespec_to_time(v.atime()) as u64;
        bi.bi_mtime = fs.timespec_to_time(v.mtime()) as u64;
        bi.bi_ctime = fs.timespec_to_time(v.ctime()) as u64;
    }

    /// fold_times(), written in @t: for a write of this inode later in @t,
    /// whose peek sees it.
    pub(crate) fn fold_times_trans(&self, t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
        let mut iter = BtreeIter::uninit(t.trans());
        let mut bi = inode::peek(t, &mut iter, self.inum(), BtreeIterFlags::INTENT)?;
        self.fold_times(&mut bi);
        inode::write(t, &mut iter, &mut bi)
    }

    /// Writeback of it scheduled, writeback_timeout from now.
    pub(crate) fn queue_writeback(&self) {
        let fs = self.fs();
        let timeout = fs.opts.writeback_timeout.get();

        if timeout != 0 {
            unsafe { self.ei_writeback_timer.queue(&fs.vfs.writeback_wq, usize::from(timeout) * HZ) };
        }
    }

    /// Forget the cached allocation state, as anything that deallocates must:
    /// see InodeReserved.
    pub fn clear_reserved(&self) {
        let mut r = self.ei_reserved.lock();
        let range = r.published();
        range.ei_reserved_start.set(0);
        range.ei_reserved_end.set(0);
    }

    pub fn update_lock(&self) -> UpdateLock<'_> {
        UpdateLock { inode: self, _guard: self.ei_update_lock.lock() }
    }

    /// Take the update locks of @inodes, together, in lock_order().
    pub fn update_lock_all<const N: usize>(inodes: [&InodeInfo; N]) -> UpdateLocks<'_, N> {
        UpdateLocks(lock_order(inodes).map(|(subclass, inode)| {
            UpdateLock { inode, _guard: inode.ei_update_lock.lock_nested(subclass) }
        }).collect())
    }

    /// Block page cache adds to @inodes, together, in lock_order().
    pub fn pagecache_block_all<const N: usize>(inodes: [&InodeInfo; N])
        -> ArrayVec<TwoStateGuard<'_, PagecacheBlock>, N>
    {
        lock_order(inodes).map(|(_, inode)| inode.ei_pagecache_lock.lock()).collect()
    }
}

/// Have @subvols' inodes evicted as soon as they're unused, and wait for them
/// all to be freed.
pub fn evict_subvolume_inodes(fs: &Fs, subvols: &[u32]) {
    // Initially, we scan for inodes without I_DONTCACHE, then mark them to be
    // pruned with d_mark_dontcache().
    //
    // Once we've had a clean pass where we didn't find any inodes without
    // I_DONTCACHE, we wait for them to be freed.
    //
    // The scan tracks position by slot, so we can drop rcu around the dcache
    // work (held safe by our igrab ref) and resume from the next slot.
    let ours = |inode: &InodeInfo| subvols.contains(&(inode.inum().subvol as u32));

    let mut pos = 0;
    loop {
        let next = {
            let g = read_lock();
            vfs_inodes_from(fs, pos, &g).find_map(|(i, inode)| {
                let v = inode.vfs();
                (ours(inode) && v.state_once() & c::I_DONTCACHE == 0).then(|| v.igrab()).flatten()
                    .map(|r| (i, r))
            })
        };
        let Some((i, inode)) = next else { break };
        inode.mark_dontcache();
        inode.prune_aliases();
        pos = i + 1;
    }

    loop {
        let mut found = false;
        let mut pos = 0;
        loop {
            let next = {
                let g = read_lock();
                vfs_inodes_from(fs, pos, &g).find(|(_, inode)| ours(inode))
                    .map(|(i, inode)| (i, NonNull::from(inode.vfs())))
            };
            let Some((i, inode)) = next else { break };
            found = true;
            VfsInode::wait_new_bit(inode, || {
                let g = read_lock();
                vfs_inodes_from(fs, i, &g).next().is_some_and(|(j, still)| j == i && NonNull::from(still.vfs()) == inode)
            }, isize::MAX); // MAX_SCHEDULE_TIMEOUT
            pos = i + 1;
        }
        if !found {
            break;
        }
    }
}

/// The filesystem's inodes, from slot @pos on.
fn vfs_inodes_from<'g>(fs: &'g Fs, pos: usize, g: &'g ReadGuard)
    -> impl Iterator<Item = (usize, &'g InodeInfo)> + 'g
{
    // freed a grace period after evict takes them off, as the VFS frees inodes
    fs.vfs.inodes.items_from(pos, g).map(|(i, p)| (i, unsafe { p.as_ref() }))
}

/// @inodes in address order, each once, with its lockdep subclass - so two
/// lockers of the same inodes can't deadlock.
fn lock_order<const N: usize>(mut inodes: [&InodeInfo; N]) -> impl Iterator<Item = (u32, &InodeInfo)> {
    inodes.sort_unstable_by_key(|i| core::ptr::from_ref(*i));

    // C's subclasses start at 1
    (0..N).filter(move |&i| i == 0 || !core::ptr::eq(inodes[i], inodes[i - 1]))
          .map(move |i| (i as u32 + 1, inodes[i]))
}

pub struct UpdateLocks<'i, const N: usize>(ArrayVec<UpdateLock<'i>, N>);

impl<'i, const N: usize> UpdateLocks<'i, N> {
    /// @inode's, one of those locked.
    pub fn get(&self, inode: &InodeInfo) -> &UpdateLock<'i> {
        self.0.iter().find(|l| core::ptr::eq(l.inode, inode))
            .expect("UpdateLocks::get() of an inode not locked")
    }
}


/// ei_update_lock, held.
pub struct UpdateLock<'i> {
    inode:  &'i InodeInfo,
    _guard: ExclusiveGuard<'i, Mutex>,
}

impl<'i> UpdateLock<'i> {
    /// Move the inode's quota charges - its usage, i_blocks +
    /// ei_quota_reserved, and the inode itself - to @qid, in the quota types
    /// of @qtypes that are accounted.
    pub fn quota_transfer(&self, qid: Qid, qtypes: u32, mode: QuotaAcctMode)
        -> Result<(), BchError>
    {
        let inode = self.inode;
        let fs = inode.fs();
        // @qid's entries, made before ei_quota, a spinlock: ours are there
        quota::prealloc(fs, qid)?;
        let w = &mut QuotaWarnings::new(fs);
        let mut q = inode.ei_quota.lock();

        let qtypes = quota::set_qtypes(qtypes & quota::enabled_qtypes(fs))
            .filter(|&i| qid.q[i] != q.ei_qid.q[i])
            .fold(0, |m, i| m | 1 << i);
        if qtypes == 0 {
            return Ok(());
        }

        // Quotas don't count snapshot inodes: only the qid changes.
        if !inode.ei_flags.get(InodeInfoFlag::EI_INODE_SNAPSHOT) {
            // blocks(): stable, under ei_quota_lock
            quota::transfer(fs, qtypes, qid, q.ei_qid, inode.vfs().blocks() + q.ei_quota_reserved, mode, w)?;
        }

        for i in quota::set_qtypes(qtypes) {
            q.ei_qid.q[i] = qid.q[i];
        }
        Ok(())
    }

    /// Move the inode to project @projid, quota and all.
    pub fn set_projid(&self, projid: u32) -> Result<(), BchError> {
        let mut qid = self.inode.ei_quota.lock().ei_qid;
        qid.q[QuotaType::QTYP_PRJ] = projid;

        self.quota_transfer(qid, 1 << QuotaType::QTYP_PRJ.0, QuotaAcctMode::PREALLOC)
    }

    /// Move the inode to @dir's project, if moving it into @dir would: it has
    /// no project of its own, and @dir's is different.
    pub fn inherit_project(&self, dir: &InodeInfo) -> Result<(), BchError> {
        let project = InodeOpt::of(InodeOptId::project);
        let (bi, dir_bi) = (self.inode.inode(), dir.inode());
        if project.is_own(&bi) || project.get(&dir_bi) == project.get(&bi) {
            return Ok(());
        }
        // dir's quota lock dropped before ours is taken: the same class, either order
        let projid = dir.ei_quota.lock().ei_qid.q[QuotaType::QTYP_PRJ];
        self.set_projid(projid)
    }

    /// Change the inode with @set, in a transaction of its own that's
    /// committed, then update the VFS inode - @fields, the ATTR_* that
    /// changed. @set runs in each attempt, on the inode as it is in the btree,
    /// its times the VFS's.
    pub fn write_inode<F>(&self, fields: u32, mut set: F) -> Result<(), BchError>
    where
        F: FnMut(&TransAttempt<'_, '_>, &mut InodeUnpacked) -> Result<(), BchError>,
    {
        self.write_inode_if(fields, |t, bi| set(t, bi).map(|()| true)).map(|_| ())
    }

    /// write_inode(), but @set says whether there's anything to write: if
    /// not, nothing's written or updated. Whether it wrote.
    pub fn write_inode_if<F>(&self, fields: u32, mut set: F) -> Result<bool, BchError>
    where
        F: FnMut(&TransAttempt<'_, '_>, &mut InodeUnpacked) -> Result<bool, BchError>,
    {
        let inode = self.inode;
        let fs = inode.fs();
        let trans = crate::btree_trans!(fs);
        // Outside the attempt: the propagate it starts is finished after the
        // commit
        let mut opt_change = InodeOptChange::new();

        let wrote = trans.lockrestart_do(|t| {
            opt_change = InodeOptChange::new();

            let mut iter = BtreeIter::uninit(t.trans());
            let mut bi = inode::__peek(t, &mut iter, inode.inum(), BtreeIterFlags::INTENT,
                                       Some(crate::here!()))?;
            let old_r = inode::reconcile_opts_get(fs, &bi);

            inode.fold_times(&mut bi);
            if !set(t, &mut bi)? {
                return Ok(false);
            }

            // iter's snapshot, not bi_snapshot: peek can find the inode at an
            // ancestor, but it's written here
            opt_change.record(t, &old_r, &bi, iter.snapshot())?;
            inode::write(t, &mut iter, &mut bi)?;
            t.commit(None, CommitFlags::NO_ENOSPC)?;

            inode.update_after_write(t.trans(), &bi, fields);
            Ok(true)
        })?;

        if wrote {
            opt_change.finish(&trans)?;
        }
        Ok(wrote)
    }

    /// i_size to @new_size, and the times of @fields to now.
    pub fn write_size(&self, new_size: u64, fields: u32) -> Result<(), BchError> {
        let now = self.inode.fs().current_time();

        self.write_inode(fields, |_, bi| {
            bi.bi_size = new_size;
            if fields & ATTR_ATIME != 0 {
                bi.bi_atime = now;
            }
            if fields & ATTR_MTIME != 0 {
                bi.bi_mtime = now;
            }
            if fields & ATTR_CTIME != 0 {
                bi.bi_ctime = now;
            }
            Ok(())
        })
    }
}

/// i_blocks gone below 0 - @blocks, less @sectors - and clamped there.
#[must_use = "an underflow is logged, once ei_quota is unlocked"]
struct IBlocksUnderflow {
    blocks:  u64,
    sectors: i64,
}

/// i_blocks and quota reservations, under ei_quota_lock: the _locked() forms
/// take it as @q. A spinlock - a folio's dirtying charges the quota, and
/// mustn't sleep - so what's found under it that would is done after: quota
/// warnings by a QuotaWarnings made first, an underflow's log by the caller.
impl InodeInfo {
    /// i_blocks changed by @sectors, and the quota charged - from @quota_res
    /// for an allocation it reserved.
    fn i_sectors_acct_locked(&self, q: &mut InodeQuota, quota_res: Option<&mut QuotaRes>,
                             mut sectors: i64, w: &mut QuotaWarnings<'_>) -> Option<IBlocksUnderflow> {
        let blocks = self.vfs().blocks();
        let underflow = ((blocks as i64) + sectors < 0).then(|| {
            let u = IBlocksUnderflow { blocks, sectors };
            sectors = -(blocks as i64);
            u
        });

        self.vfs().set_blocks(blocks.wrapping_add_signed(sectors));

        if !cfg!(CONFIG_BCACHEFS_QUOTA) || self.ei_flags.get(InodeInfoFlag::EI_INODE_SNAPSHOT) {
            return underflow;
        }

        match quota_res {
            Some(res) if sectors > 0 => {
                let sectors = sectors as u64;
                assert!(sectors <= res.sectors && sectors <= q.ei_quota_reserved,
                        "i_sectors_acct: {sectors} sectors from a quota reservation of {}, {} reserved",
                        res.sectors, q.ei_quota_reserved);

                res.sectors -= sectors;
                q.ei_quota_reserved -= sectors;
            }
            _ => {
                let _ = quota::acct(self.fs(), q.ei_qid, quota::Q_SPC, sectors,
                                    QuotaAcctMode::WARN, w);
            }
        }
        underflow
    }

    pub fn i_sectors_acct(&self, quota_res: Option<&mut QuotaRes>, sectors: i64) {
        if sectors != 0 {
            let w = &mut QuotaWarnings::new(self.fs());
            let underflow = self.i_sectors_acct_locked(&mut self.ei_quota.lock(), quota_res, sectors, w);
            if let Some(u) = underflow {
                self.i_blocks_underflow(u);
            }
        }
    }

    /// An underflow i_sectors_acct_locked() clamped, logged with ei_quota
    /// unlocked: an fsck error's count takes a mutex.
    #[cold]
    #[inline(never)]
    fn i_blocks_underflow(&self, IBlocksUnderflow { blocks, sectors }: IBlocksUnderflow) {
        let fs = self.fs();
        let inum = self.inum();
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "subvol {} inode {} i_blocks underflow: {} + {} < 0 (ondisk {})",
             inum.subvol, inum.inum, blocks, sectors, self.inode().bi_sectors as i64);

        let log = count_fsck_err(fs, id::vfs_inode_i_blocks_underflow, &mut msg);
        msg.set_suppressed(!log);
    }

    pub(crate) fn quota_reservation_add(&self, res: &mut QuotaRes, sectors: u64, check_enospc: bool)
        -> Result<(), BchError>
    {
        if !cfg!(CONFIG_BCACHEFS_QUOTA) || self.ei_flags.get(InodeInfoFlag::EI_INODE_SNAPSHOT) {
            return Ok(());
        }

        let w = &mut QuotaWarnings::new(self.fs());
        let mut q = self.ei_quota.lock();
        let mode = if check_enospc {
            QuotaAcctMode::PREALLOC
        } else {
            QuotaAcctMode::NOCHECK
        };
        quota::acct(self.fs(), q.ei_qid, quota::Q_SPC, sectors as i64, mode, w)?;

        q.ei_quota_reserved += sectors;
        res.sectors += sectors;
        Ok(())
    }

    fn quota_reservation_put_locked(&self, q: &mut InodeQuota, res: &mut QuotaRes,
                                    w: &mut QuotaWarnings<'_>) {
        if !cfg!(CONFIG_BCACHEFS_QUOTA) {
            return;
        }

        assert!(res.sectors <= q.ei_quota_reserved,
                "quota_reservation_put: a reservation of {} sectors, {} reserved",
                res.sectors, q.ei_quota_reserved);

        let _ = quota::acct(self.fs(), q.ei_qid, quota::Q_SPC, -(res.sectors as i64),
                            QuotaAcctMode::PREALLOC, w);
        q.ei_quota_reserved -= res.sectors;
        res.sectors = 0;
    }

    pub(crate) fn quota_reservation_put(&self, res: &mut QuotaRes) {
        if cfg!(CONFIG_BCACHEFS_QUOTA) && res.sectors != 0 {
            let w = &mut QuotaWarnings::new(self.fs());
            self.quota_reservation_put_locked(&mut self.ei_quota.lock(), res, w);
        }
    }

    /// i_sectors_acct() from @res, and what's left of it put, under one hold
    /// of the quota lock: a DIO write's, as each of its bios is written.
    pub(crate) fn i_sectors_acct_put(&self, res: &mut QuotaRes, sectors: i64) {
        let w = &mut QuotaWarnings::new(self.fs());
        let underflow = {
            let mut q = self.ei_quota.lock();
            let underflow = self.i_sectors_acct_locked(&mut q, Some(&mut *res), sectors, w);
            self.quota_reservation_put_locked(&mut q, res, w);
            underflow
        };
        if let Some(u) = underflow {
            self.i_blocks_underflow(u);
        }
    }

    /// A direct write started: inode_dio_begin(), and the page cache blocked,
    /// till the DioStarted is dropped.
    pub(crate) fn dio_start(&self) -> DioStarted {
        unsafe { c::inode_dio_begin(self.v.as_ptr()) };
        self.ei_pagecache_lock.lock_raw(<PagecacheBlock as TwoState>::S);
        DioStarted(NonNull::from(self))
    }

    /// An empty quota reservation of @self's: added to, and put when dropped.
    pub fn quota_reservation(&self) -> QuotaReservation<'_> {
        QuotaReservation { inode: self, res: QuotaRes::default() }
    }
}

/// A direct write in flight on an inode: its i_dio_count, and the page cache
/// blocked. Owned, borrowing nothing, so it outlives the call that started
/// the write: the count is a ref on the inode - dio_wait() waits it out -
/// and so on its filesystem.
pub(crate) struct DioStarted(NonNull<InodeInfo>);

// ended by whichever thread finishes the write, as C's are
unsafe impl Send for DioStarted {}

impl DioStarted {
    pub(crate) fn inode(&self) -> &InodeInfo {
        unsafe { self.0.as_ref() }
    }
}

impl Drop for DioStarted {
    fn drop(&mut self) {
        let inode = self.inode();
        inode.ei_pagecache_lock.unlock_raw(<PagecacheBlock as TwoState>::S);
        /* the count is our ref on the inode: last */
        unsafe { c::inode_dio_end(inode.v.as_ptr()) };
    }
}

pub struct QuotaReservation<'i> {
    inode: &'i InodeInfo,
    res:   QuotaRes,
}

impl<'i> QuotaReservation<'i> {
    pub fn inode(&self) -> &'i InodeInfo {
        self.inode
    }

    pub fn add(&mut self, sectors: u64, check_enospc: bool) -> Result<(), BchError> {
        self.inode.quota_reservation_add(&mut self.res, sectors, check_enospc)
    }
}

impl Deref for QuotaReservation<'_> {
    type Target = QuotaRes;

    fn deref(&self) -> &QuotaRes {
        &self.res
    }
}

impl DerefMut for QuotaReservation<'_> {
    fn deref_mut(&mut self) -> &mut QuotaRes {
        &mut self.res
    }
}

impl Drop for QuotaReservation<'_> {
    fn drop(&mut self) {
        self.inode.quota_reservation_put(&mut self.res)
    }
}
