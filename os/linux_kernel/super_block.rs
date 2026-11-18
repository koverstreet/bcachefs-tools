// SPDX-License-Identifier: GPL-2.0

//! The VFS's struct super_block for a filesystem type T (fs_type.rs).
//! Generic over what its s_fs_info points to, T's SuperData::Target. Also
//! the three states mount has one in, all with s_umount held for writing, as
//! sget returns it:
//!
//! - NewSuper: one sget made, not yet filled, no root. fill() makes it
//!   mountable, into_root() the mount's root.
//! - LockedSuper: an existing one sget found, with an active reference.
//! - Root: what get_tree hands back, the root dentry. vfs_get_tree() marks
//!   its superblock born, and the mount's caller unlocks it.
//!
//! Dropping any of them deactivates the superblock - deactivate_locked_super()
//! - which for a NewSuper ends in kill_sb, dropping its data. That's the one
//!   error path, whatever mount was doing when it failed.
//!
//! vfs/super_block.rs is bcachefs's own: SuperBlock<Fs>, and its ops.

use core::ffi::{c_int, CStr};
use kernel::ffi::c_ulong;
use core::marker::PhantomData;
use core::ptr::NonNull;
use kernel::sync::SetOnce;

use crate::util::backref::BackRef;

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::locking::{RwSem, SharedGuard};
use crate::os::dentry::Dentry;
use crate::os::seq_file::SeqFile;
use crate::os::vfs_inode::{ILocked, VfsInode};
use crate::os::xattr::XattrHandlers;
use crate::types::CStatic;
use crate::util::ffi::Opaque;
use kernel::error::{code::ENOMEM, to_result, Result};
use kernel::sync::aref::{ARef, AlwaysRefCounted};

use super::fs_type::{FsType, SuperData};
use super::ops::{from_raw, SelfOf};

#[repr(transparent)]
pub struct SuperBlock<T>(Opaque<c::super_block>, PhantomData<T>);

/// A superblock held against freezing for a page fault's write: until it's
/// dropped, sb_end_pagefault().
#[cfg(kernel)]
pub struct PagefaultWrite<'a>(*mut c::super_block, PhantomData<&'a ()>);

#[cfg(kernel)]
impl Drop for PagefaultWrite<'_> {
    fn drop(&mut self) {
        unsafe { c::sb_end_pagefault(self.0) }
    }
}

impl<T> SuperBlock<T> {
    /// # Safety
    /// @sb is live for 'a, and its s_fs_info is a T: set at creation, before
    /// the superblock is published (FsContext::create_super()), and live until
    /// kill_sb.
    pub unsafe fn from_raw<'a>(sb: *mut c::super_block) -> &'a Self {
        unsafe { &*sb.cast::<Self>() }
    }

    pub fn as_ptr(&self) -> *mut c::super_block {
        self.0.as_ptr()
    }

    /// Readahead's size, in pages, for its bdi: a MountedOn's, whose bdi is
    /// its own.
    pub fn set_ra_pages(&self, pages: c_ulong) {
        unsafe { (*(*self.as_ptr()).s_bdi).ra_pages = pages };
    }

    /// The LRU its inodes' memory is charged to: KmemCache::alloc_inode()'s.
    pub(crate) fn inode_lru(&self) -> *mut c::list_lru {
        unsafe { &raw mut (*self.as_ptr()).s_inode_lru }
    }

    /// What s_fs_info points to.
    pub fn data(&self) -> &T {
        unsafe { &*(*self.as_ptr()).s_fs_info.cast::<T>() }
    }

    pub fn dev(&self) -> c::dev_t {
        unsafe { (*self.as_ptr()).s_dev }
    }

    pub fn flags(&self) -> c_ulong {
        unsafe { (*self.as_ptr()).s_flags }
    }

    pub fn rdonly(&self) -> bool {
        self.flags() & c::SB_RDONLY as c_ulong != 0
    }

    /// Held against freezing while a page fault writes: sb_start_pagefault().
    #[cfg(kernel)]
    pub fn start_pagefault(&self) -> PagefaultWrite<'_> {
        unsafe { c::sb_start_pagefault(self.as_ptr()) };
        PagefaultWrite(self.as_ptr(), PhantomData)
    }

    /// sync_filesystem(), if s_umount can be had for reading without waiting:
    /// whether it was.
    #[cfg(kernel)]
    pub fn try_sync(&self) -> bool {
        let s_umount = unsafe { &raw mut (*self.as_ptr()).s_umount };
        if unsafe { c::down_read_trylock(s_umount) } == 0 {
            return false;
        }
        unsafe {
            c::sync_filesystem(self.as_ptr());
            c::up_read(s_umount);
        }
        true
    }

    pub fn umount(&self) -> &crate::locking::Locked<RwSem> {
        // RwSem is the kernel's rw_semaphore, s_umount one
        crate::locking::Locked::from_c(unsafe { &*(&raw const (*self.as_ptr()).s_umount).cast() })
    }

    /// Its dirty inodes written back, and waited on: sync_inodes_sb().
    pub fn sync_inodes(&self, _umount: &SharedGuard<'_, RwSem>) {
        unsafe { c::sync_inodes_sb(self.as_ptr()) }
    }

    /// Whether @uid and @gid are ids in its user namespace:
    /// kuid_has_mapping(), kgid_has_mapping().
    pub fn maps_ids(&self, uid: c::kuid_t, gid: c::kgid_t) -> bool {
        let ns = unsafe { (*self.as_ptr()).s_user_ns };
        unsafe { c::kuid_has_mapping(ns, uid) && c::kgid_has_mapping(ns, gid) }
    }

    /// Frozen, by @who - writes stopped and what's dirty synced: freeze_super().
    pub fn freeze(&self, who: c::freeze_holder) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::freeze_super(self.as_ptr(), who, core::ptr::null()) })
    }

    /// freeze()'s undone: thaw_super(). An error is a freeze @who didn't hold.
    pub fn thaw(&self, who: c::freeze_holder) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::thaw_super(self.as_ptr(), who, core::ptr::null()) })
    }
}

from_raw! {
    &'a SuperBlock<T> [T] = *mut c::super_block => |r| SuperBlock::from_raw(r);
}

// s_fs_info is set at mount, before the superblock has an inode, to the T
// that implements its traits: SelfOf's contract

impl<T: 'static> SelfOf<T> for *mut c::super_block {
    type Out = T;
    unsafe fn self_of<'a>(&self) -> &'a T { unsafe { &*(**self).s_fs_info.cast::<T>() } }
}

impl<T: 'static> SelfOf<T> for *mut c::inode {
    type Out = T;
    unsafe fn self_of<'a>(&self) -> &'a T { unsafe { SelfOf::<T>::self_of(&(**self).i_sb) } }
}

impl<T: 'static> SelfOf<T> for *mut c::dentry {
    type Out = T;
    unsafe fn self_of<'a>(&self) -> &'a T { unsafe { SelfOf::<T>::self_of(&(**self).d_sb) } }
}

crate::vfs_ops! {
    /// A superblock's operations, super_operations: Self is what s_fs_info
    /// points to, and each op gets it as &self, but free_inode(), which can
    /// run after the filesystem's freed.
    pub(crate) trait SuperOps for c::super_operations, SuperVTable<T: SuperOps> = T;
    type Inode: IsInode<Super = Self>;

    fn alloc_inode(&self = sb, sb: &SuperBlock<Self>) -> Result<NonNull<c::inode>, BchError>;
    /// The inode's memory, the VFS done with it - an RCU callback after
    /// eviction, which can run after the filesystem's freed: no &self.
    fn free_inode(inode: NonNull<Self::Inode>);
    /// Not always referenced - iput_final() writes one back with i_count 0
    /// - but I_SYNC or I_WILL_FREE keeps it from eviction meanwhile.
    fn write_inode(&self = inode, inode: &Self::Inode, wbc: *mut c::writeback_control)
        -> Result<(), BchError>;
    /// I_FREEING: ours to evict, the superblock still up.
    fn evict_inode(&self = inode, inode: &Self::Inode);
    fn sync_fs(&self = sb, sb: &SuperBlock<Self>, wait: c_int) -> Result<(), BchError>;
    fn statfs(&self = dentry, dentry: Dentry<'_, Self::Inode>, buf: &mut c::kstatfs) -> Result<(), BchError>;
    fn show_devname(&self = root, m: &SeqFile, root: Dentry<'_, Self::Inode>) -> Result<(), BchError>;
    fn show_options(&self = root, m: &SeqFile, root: Dentry<'_, Self::Inode>) -> Result<(), BchError>;
    /// The VFS's last op on the superblock.
    fn put_super(&self = sb, sb: &SuperBlock<Self>);
    fn freeze_fs(&self = sb, sb: &SuperBlock<Self>) -> Result<(), BchError>;
    fn unfreeze_fs(&self = sb, sb: &SuperBlock<Self>) -> Result<(), BchError>;
}

/// A superblock from sget with s_umount held for writing, its reference: the
/// active one, for an existing superblock; the first, for a new one.
struct Locked(NonNull<c::super_block>);

impl Locked {
    fn sb<D>(&self) -> &SuperBlock<D> {
        unsafe { SuperBlock::from_raw(self.0.as_ptr()) }
    }

    /// Kept locked and referenced: the VFS's now.
    fn into_raw(self) -> NonNull<c::super_block> {
        core::mem::ManuallyDrop::new(self).0
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        unsafe { c::deactivate_locked_super(self.0.as_ptr()) }
    }
}

/// Strings into the superblock's fixed-size, NUL-terminated name fields:
/// strscpy()'s truncation.
fn copy_str<const N: usize>(dst: &mut [u8; N], src: &CStr) {
    let b = src.to_bytes();
    let n = b.len().min(N - 1);

    dst[..n].copy_from_slice(&b[..n]);
    dst[n] = 0;
}

/// A struct inode, as the filesystem has its own - the only kind on Super's
/// superblocks, its SuperOps::Inode: what fill() makes the root dentry of,
/// from a reference, and what the VFS's ops pass it as.
///
/// # Safety
/// A &Self points to a struct inode on a SuperBlock<Super>, and its
/// AlwaysRefCounted reference is one on i_count - ihold(), iput().
pub unsafe trait IsInode: AlwaysRefCounted {
    type Super: SuperOps<Inode = Self>;

    fn sb(&self) -> &SuperBlock<Self::Super>
        where Self: Sized
    {
        unsafe { SuperBlock::from_raw((*core::ptr::from_ref(self).cast::<c::inode>()).i_sb) }
    }

    /// A reference, taken under its i_lock - none, if the VFS is freeing it:
    /// VfsInode::iget()'s, as one of Self's.
    fn iget(&self, i_lock: &ILocked<'_>) -> Option<ARef<Self>>
        where Self: Sized
    {
        let v = unsafe { &*core::ptr::from_ref(self).cast::<VfsInode>() };
        v.iget(i_lock).map(|r| unsafe { ARef::from_raw(ARef::into_raw(r).cast()) })
    }
}

from_raw! {
    &'a I [I: IsInode]         = *mut c::inode => |r| &*r.cast();
    /// A parent, which may not be given.
    Option<&'a I> [I: IsInode] = *mut c::inode => |r| r.cast::<I>().as_ref();
    /// One the VFS is done with, to free: not borrowed, nor its superblock.
    NonNull<I> [I: IsInode]    = *mut c::inode => |r| NonNull::new(r).expect("an inode").cast();
}

// ── A new superblock ─────────────────────────────────────────────────────

/// A superblock sget made: s_umount held for writing, no root, not yet born -
/// SB_BORN isn't set until get_tree returns. Its fields are set here, through
/// these, then fill() gives it its root.
///
/// The fields set are ours until it's born, but the superblock isn't: sget
/// put it on super_blocks before returning it, and sync() and the rest of
/// iterate_supers() take its s_count and try its s_umount meanwhile. So each
/// is written through the pointer, never a &mut of the whole struct.
pub struct NewSuper<T: FsType>(Locked, PhantomData<T>);

type Data<T> = <<T as FsType>::Data as SuperData>::Target;

/// The superblock a T is mounted as, a backref from the T it owns: set once,
/// by NewSuper::setup_bdi(), where the T's SuperData::mounted_on() says.
pub type MountedOn<T> = SetOnce<BackRef<SuperBlock<T>>>;

// The VFS's, shared by every task with the filesystem: its fields are C's,
// changed through its own locks
unsafe impl<T: Sync> Sync for SuperBlock<T> {}

impl<T: FsType> NewSuper<T> {
    /// # Safety
    /// @sb is a superblock sget_fc() created, s_umount held for writing, its
    /// s_fs_info set to a T::Data.
    pub(super) unsafe fn from_raw(sb: NonNull<c::super_block>) -> Self {
        NewSuper(Locked(sb), PhantomData)
    }

    pub fn sb(&self) -> &SuperBlock<Data<T>> {
        self.0.sb()
    }

    /// The superblock, for writing its fields: see NewSuper.
    fn raw(&self) -> *mut c::super_block {
        self.0 .0.as_ptr()
    }

    /// @bytes a power of two.
    pub fn set_blocksize(&self, bytes: u32) {
        let sb = self.raw();
        unsafe {
            (*sb).s_blocksize = bytes as c_ulong;
            (*sb).s_blocksize_bits = bytes.ilog2() as u8;
        }
    }

    pub fn set_maxbytes(&self, max: i64) {
        unsafe { (*self.raw()).s_maxbytes = max };
    }

    pub fn set_magic(&self, magic: c_ulong) {
        unsafe { (*self.raw()).s_magic = magic };
    }

    /// Timestamp granularity in nanoseconds, and the range representable.
    pub fn set_time(&self, gran: u32, min: i64, max: i64) {
        let sb = self.raw();
        unsafe {
            (*sb).s_time_gran = gran;
            (*sb).s_time_min = min;
            (*sb).s_time_max = max;
        }
    }

    /// Its ops, before it has an inode: Data<T>'s, whose inodes are its
    /// SuperOps::Inode.
    pub fn set_ops(&self, ops: &'static CStatic<c::super_operations, Data<T>>)
        where Data<T>: SuperOps
    {
        unsafe { (*self.raw()).s_op = &**ops };
    }

    pub fn set_export_ops(&self, ops: &'static c::export_operations) {
        // codegen's export_operations, s_export_op the kernel crate's: the same struct
        unsafe { (*self.raw()).s_export_op = (ops as *const c::export_operations).cast() };
    }

    /// Its xattr handlers: its inodes'.
    pub fn set_xattr_handlers<const N: usize>(&self, h: &'static XattrHandlers<<Data<T> as SuperOps>::Inode, N>)
        where Data<T>: SuperOps
    {
        unsafe { (*self.raw()).s_xattr = h.as_ptr().cast() };
    }

    pub fn set_quotactl(&self, ops: &'static c::quotactl_ops, types: u32) {
        let sb = self.raw();
        unsafe {
            (*sb).s_qcop = (ops as *const c::quotactl_ops).cast();
            (*sb).s_quota_types = types as _;
        }
    }

    pub fn set_uuid(&self, uuid: &[u8; 16]) {
        unsafe { c::super_set_uuid(self.raw(), uuid.as_ptr(), uuid.len() as u32) }
    }

    /// The sysfs name: the UUID, as a multi-device filesystem has it.
    pub fn set_sysfs_name_uuid(&self) {
        unsafe { c::super_set_sysfs_name_uuid(self.raw()) }
    }

    pub fn set_sysfs_name(&self, name: &CStr) {
        copy_str(unsafe { &mut (*self.raw()).s_sysfs_name }, name);
    }

    /// s_id, what the VFS's messages name the superblock by.
    pub fn set_id(&self, id: &CStr) {
        copy_str(unsafe { &mut (*self.raw()).s_id }, id);
    }

    /// The superblock shrinker's seeks: 0 for a filesystem with its own cache
    /// policy.
    pub fn set_shrinker_seeks(&self, seeks: c_int) {
        unsafe { (*(*self.raw()).s_shrink).seeks = seeks }
    }

    /// Give it a bdi of its own, readahead @ra_pages - and from here, the data
    /// it owns is MountedOn it: its bdi's its own, not the noop one every
    /// superblock starts with, for set_ra_pages().
    pub fn setup_bdi(&self, ra_pages: c_ulong) -> Result {
        to_result(unsafe { c::super_setup_bdi(self.raw()) })?;
        let sb = self.sb();
        sb.set_ra_pages(ra_pages);
        // the superblock owns its data, and drops it at kill_sb, before it
        // goes: the data's what holds this
        let set = <T::Data as SuperData>::mounted_on(sb.data()).populate(unsafe { BackRef::new(sb) });
        assert!(set, "a filesystem set up as a second superblock");
        Ok(())
    }

    pub fn set_flags(&self, flags: c_ulong) {
        unsafe { (*self.raw()).s_flags |= flags };
    }

    /// # Safety
    /// @map is live until kill_sb - generic_shutdown_super() is its last user.
    #[cfg(CONFIG_UNICODE)]
    pub unsafe fn set_encoding(&self, map: &crate::os::unicode::UnicodeMap) {
        unsafe { (*self.raw()).s_encoding = map.as_ptr().cast() };
    }

    /// Its root, made from @root's reference - d_make_root() - which makes it
    /// active and mountable: into_root() then.
    pub fn fill(&self, root: ARef<<Data<T> as SuperOps>::Inode>) -> Result
        where Data<T>: SuperOps
    {
        let sb = self.raw();
        debug_assert!(unsafe { (*sb).s_root }.is_null(), "a superblock is filled once");
        let dentry = unsafe { c::d_make_root(ARef::into_raw(root).as_ptr().cast()) };
        let dentry = NonNull::new(dentry).ok_or(ENOMEM)?;

        unsafe {
            (*sb).s_root = dentry.as_ptr().cast();
            (*sb).s_flags |= c::SB_ACTIVE as c_ulong;
        }
        Ok(())
    }

    /// The mount's root, filled.
    pub fn into_root(self) -> Root<T> {
        assert!(!unsafe { (*self.0 .0.as_ptr()).s_root }.is_null(), "a superblock fill()ed");
        Root::new(self.0)
    }
}

// ── An existing superblock ───────────────────────────────────────────────

/// A superblock sget found: s_umount held for writing, and an active
/// reference. Already filled and mounted.
pub struct LockedSuper<T: FsType>(Locked, PhantomData<T>);

impl<T: FsType> LockedSuper<T> {
    /// # Safety
    /// @sb is a superblock sget_fc() found and returned locked: s_umount held
    /// for writing, an active reference taken.
    pub(super) unsafe fn from_raw(sb: NonNull<c::super_block>) -> Self {
        LockedSuper(Locked(sb), PhantomData)
    }

    pub fn sb(&self) -> &SuperBlock<Data<T>> {
        self.0.sb()
    }

    /// Mounted again: its root, with a reference.
    pub fn into_root(self) -> Root<T> {
        Root::new(self.0)
    }
}

// ── A reconfigure's ──────────────────────────────────────────────────────

/// The superblock a reconfigure is for, s_umount held for writing by
/// reconfigure_super(): what may sync it and flip its read-only flag.
pub struct ReconfSuper<'a, D>(&'a SuperBlock<D>);

impl<'a, D> ReconfSuper<'a, D> {
    /// # Safety
    /// s_umount is held for writing, for 'a.
    pub(super) unsafe fn new(sb: &'a SuperBlock<D>) -> Self {
        ReconfSuper(sb)
    }

    pub fn sb(&self) -> &'a SuperBlock<D> {
        self.0
    }

    pub fn sync(&self) -> Result {
        to_result(unsafe { c::sync_filesystem(self.0.as_ptr()) })
    }

    pub fn set_rdonly(&self, ro: bool) {
        // SAFETY: s_flags is s_umount's
        let flags = unsafe { &mut (*self.0.as_ptr()).s_flags };
        if ro {
            *flags |= c::SB_RDONLY as c_ulong;
        } else {
            *flags &= !(c::SB_RDONLY as c_ulong);
        }
    }
}

// ── What get_tree hands back ─────────────────────────────────────────────

/// A superblock's root dentry, with a reference, the superblock still
/// s_umount-locked for writing: get_tree's result, fc->root. vfs_get_tree()
/// marks the superblock born, and its caller unlocks it.
pub struct Root<T: FsType> {
    dentry: NonNull<c::dentry>,
    sb:     Locked,
    _t:     PhantomData<T>,
}

impl<T: FsType> Root<T> {
    fn new(sb: Locked) -> Self {
        // SAFETY: a filled superblock, locked: its root is set and stays.
        // dget(), a static inline: its reference is the lockref's
        let dentry = NonNull::new(unsafe { (*sb.0.as_ptr()).s_root })
            .expect("a filled superblock's root");
        unsafe { c::lockref_get(&raw mut (*dentry.as_ptr()).d_lockref) };
        Root { dentry: dentry.cast(), sb, _t: PhantomData }
    }

    pub fn as_ptr(&self) -> *mut c::dentry {
        self.dentry.as_ptr()
    }

    pub fn sb(&self) -> &SuperBlock<Data<T>> {
        self.sb.sb()
    }

    /// The root, for fc->root: the VFS's now, superblock lock and all.
    pub(super) fn into_raw(self) -> NonNull<c::dentry> {
        let this = core::mem::ManuallyDrop::new(self);
        let dentry = this.dentry;
        // SAFETY: moved out of a ManuallyDrop that's never touched again
        unsafe { core::ptr::read(&this.sb) }.into_raw();
        dentry
    }
}

impl<T: FsType> Drop for Root<T> {
    /// fc_drop_locked(): the dentry's reference, then the superblock's.
    fn drop(&mut self) {
        unsafe { c::dput(self.dentry.as_ptr().cast()) }
    }
}
