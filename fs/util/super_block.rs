// SPDX-License-Identifier: GPL-2.0

//! The VFS's struct super_block for a filesystem type T (util/fs_type.rs).
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

use crate::c;
use crate::util::ffi::Opaque;
use kernel::error::{code::ENOMEM, to_result, Result};
use kernel::sync::aref::{ARef, AlwaysRefCounted};

use super::fs_type::{FsType, SuperData};

#[repr(transparent)]
pub struct SuperBlock<T>(Opaque<c::super_block>, PhantomData<T>);

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

/// A struct inode, as the filesystem has its own: what fill() makes the root
/// dentry of, from a reference.
///
/// # Safety
/// A &Self points to a struct inode, and its AlwaysRefCounted reference is
/// one on i_count - ihold(), iput().
pub unsafe trait IsInode: AlwaysRefCounted {}

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
    fn raw(&mut self) -> *mut c::super_block {
        self.0 .0.as_ptr()
    }

    /// @bytes a power of two.
    pub fn set_blocksize(&mut self, bytes: u32) {
        let sb = self.raw();
        unsafe {
            (*sb).s_blocksize = bytes as c_ulong;
            (*sb).s_blocksize_bits = bytes.ilog2() as u8;
        }
    }

    pub fn set_maxbytes(&mut self, max: i64) {
        unsafe { (*self.raw()).s_maxbytes = max };
    }

    pub fn set_magic(&mut self, magic: c_ulong) {
        unsafe { (*self.raw()).s_magic = magic };
    }

    /// Timestamp granularity in nanoseconds, and the range representable.
    pub fn set_time(&mut self, gran: u32, min: i64, max: i64) {
        let sb = self.raw();
        unsafe {
            (*sb).s_time_gran = gran;
            (*sb).s_time_min = min;
            (*sb).s_time_max = max;
        }
    }

    /// The ops. A table, so 'static. Its functions take s_fs_info as T::Data's
    /// target: SuperVTable::<Data<T>>::OPS is the one that does.
    pub fn set_ops(&mut self, ops: &'static c::super_operations) {
        unsafe { (*self.raw()).s_op = ops };
    }

    pub fn set_export_ops(&mut self, ops: &'static c::export_operations) {
        // codegen's export_operations, s_export_op the kernel crate's: the same struct
        unsafe { (*self.raw()).s_export_op = (ops as *const c::export_operations).cast() };
    }

    /// The xattr handlers, NULL-terminated.
    ///
    /// # Safety
    /// Every pointer before the NULL is to a handler, live as long as @h.
    pub unsafe fn set_xattr_handlers<const N: usize>(&mut self, h: &'static [*const c::xattr_handler; N]) {
        debug_assert!(h.last().is_some_and(|p| p.is_null()), "xattr handlers are NULL-terminated");
        unsafe { (*self.raw()).s_xattr = h.as_ptr().cast() };
    }

    pub fn set_quotactl(&mut self, ops: &'static c::quotactl_ops, types: u32) {
        let sb = self.raw();
        unsafe {
            (*sb).s_qcop = (ops as *const c::quotactl_ops).cast();
            (*sb).s_quota_types = types as _;
        }
    }

    pub fn set_uuid(&mut self, uuid: &[u8; 16]) {
        unsafe { c::super_set_uuid(self.raw(), uuid.as_ptr(), uuid.len() as u32) }
    }

    /// The sysfs name: the UUID, as a multi-device filesystem has it.
    pub fn set_sysfs_name_uuid(&mut self) {
        unsafe { c::super_set_sysfs_name_uuid(self.raw()) }
    }

    pub fn set_sysfs_name(&mut self, name: &CStr) {
        copy_str(unsafe { &mut (*self.raw()).s_sysfs_name }, name);
    }

    /// s_id, what the VFS's messages name the superblock by.
    pub fn set_id(&mut self, id: &CStr) {
        copy_str(unsafe { &mut (*self.raw()).s_id }, id);
    }

    /// The superblock shrinker's seeks: 0 for a filesystem with its own cache
    /// policy.
    pub fn set_shrinker_seeks(&mut self, seeks: c_int) {
        unsafe { (*(*self.raw()).s_shrink).seeks = seeks }
    }

    /// Give it a bdi of its own, readahead @ra_pages.
    pub fn setup_bdi(&mut self, ra_pages: c_ulong) -> Result {
        to_result(unsafe { c::super_setup_bdi(self.raw()) })?;
        unsafe { (*(*self.raw()).s_bdi).ra_pages = ra_pages };
        Ok(())
    }

    pub fn set_flags(&mut self, flags: c_ulong) {
        unsafe { (*self.raw()).s_flags |= flags };
    }

    /// # Safety
    /// @map is live until kill_sb - generic_shutdown_super() is its last user.
    #[cfg(CONFIG_UNICODE)]
    pub unsafe fn set_encoding(&mut self, map: *mut c::unicode_map) {
        unsafe { (*self.raw()).s_encoding = map.cast() };
    }

    /// Its root, made from @root's reference - d_make_root() - which makes it
    /// active and mountable: into_root() then.
    pub fn fill<I: IsInode>(&mut self, root: ARef<I>) -> Result {
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
