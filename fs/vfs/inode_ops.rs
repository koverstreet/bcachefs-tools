// SPDX-License-Identifier: GPL-2.0

//! An inode's operations as a trait, InodeOps, declared by vfs_ops!
//! (ops.rs), as the super ops are.
//!
//! There are four tables - a directory's, a regular file's, a symlink's, and
//! everything else's - so four zero-sized types implement it, with no state:
//! each op is an associated fn. The ops every table has - getattr, setattr,
//! listxattr, the ACLs, fileattr - are provided, their HAS_* true unless an
//! impl says otherwise; the rest are each table's. A directory's, DirOps,
//! are the namespace operations, namei.rs's.
//!
//! fiemap is fiemap.c's, until it's Rust.

use core::ffi::{c_uint, CStr};
use core::mem::ManuallyDrop;

use crate::acl::{self, AclType, PosixAcl};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::ffi::{c_char, opt_bytes_mut};
use crate::util::file::File;
use crate::util::ops::IntoRaw;
use crate::xattr;
use cstruct_macros::{c_export, c_extern, vtable};

use super::dentry::Dentry;
use super::idmap::MntIdmap;
use super::inode::InodeInfo;
use super::namei::DirOps;
use super::path::Path;

c_extern! {
    // fiemap.c's, until it's Rust:
    fn bch2_fiemap(vinode: *mut c::inode, info: *mut c::fiemap_extent_info, start: u64,
                   len: u64) -> c_int;
}

crate::vfs_ops! {
    pub(crate) trait InodeOps for c::inode_operations, InodeVTable<T: InodeOps> = T;

    /// What d_splice_alias() makes of the inode @dentry names.
    fn lookup(dir: &InodeInfo, dentry: Dentry<'_>, flags: c_uint) -> *mut c::dentry;
    #[cfg(not(linux_ge = "7.3"))]
    fn create(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, mode: c::umode_t,
                  excl: bool) -> Result<(), BchError>;
    #[cfg(linux_ge = "7.3")]
    fn create(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, mode: c::umode_t)
        -> Result<(), BchError>;
    fn link(old: Dentry<'_>, dir: &InodeInfo, dentry: Dentry<'_>) -> Result<(), BchError>;
    fn unlink(dir: &InodeInfo, dentry: Dentry<'_>) -> Result<(), BchError>;
    fn symlink(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, target: &CStr)
        -> Result<(), BchError>;
    /// NULL for the dentry it was given, instantiated.
    fn mkdir(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, mode: c::umode_t)
        -> Result<(), BchError>;
    fn rmdir(dir: &InodeInfo, dentry: Dentry<'_>) -> Result<(), BchError>;
    fn mknod(idmap: MntIdmap<'_>, dir: &InodeInfo, dentry: Dentry<'_>, mode: c::umode_t,
                 rdev: c::dev_t) -> Result<(), BchError>;
    fn rename(idmap: MntIdmap<'_>, src_dir: &InodeInfo, src: Dentry<'_>,
                  dst_dir: &InodeInfo, dst: Dentry<'_>, flags: c_uint) -> Result<(), BchError>;
    /// @file being opened, its dentry the new inode's to instantiate.
    fn tmpfile(idmap: MntIdmap<'_>, dir: &InodeInfo, file: &File, mode: c::umode_t)
        -> Result<(), BchError>;

    fn getattr(idmap: MntIdmap<'_>, path: &Path, stat: &mut c::kstat, request_mask: u32,
                   _query_flags: c_uint) -> Result<(), BchError>
    {
        path.dentry().inode().getattr(idmap, stat, request_mask);
        Ok(())
    };
    fn setattr(idmap: MntIdmap<'_>, dentry: Dentry<'_>, attr: &mut c::iattr) -> Result<(), BchError>
    {
        dentry.inode().setattr(idmap, dentry, attr)
    };
    /// An RCU walk can't sleep, as a lookup may: ECHILD, to try again
    /// without.
    fn get_inode_acl(inode: &InodeInfo, ty: AclType, rcu: bool)
        -> Result<Option<PosixAcl>, BchError>
    {
        if rcu {
            return Err(c::ECHILD.into());
        }
        acl::get_acl(inode, ty)
    };
    fn set_acl(idmap: MntIdmap<'_>, dentry: Dentry<'_>, acl: Option<ManuallyDrop<PosixAcl>>,
                   ty: AclType) -> Result<(), BchError>
    {
        acl::set_acl(idmap, dentry.inode(), acl.as_deref(), ty)
    };
    fn fileattr_get(dentry: Dentry<'_>, fa: &mut c::file_kattr) -> Result<(), BchError>
    {
        dentry.inode().fileattr_get(fa);
        Ok(())
    };
    fn fileattr_set(_idmap: MntIdmap<'_>, dentry: Dentry<'_>, fa: &mut c::file_kattr)
        -> Result<(), BchError>
    {
        dentry.inode().fileattr_set(dentry, fa)
    };

    /// @dentry None in an RCU walk.
    fn get_link(dentry: Option<Dentry<'_>>, inode: &InodeInfo, done: &mut c::delayed_call)
        -> *const c_char;
    fn fiemap(inode: &InodeInfo, info: &mut c::fiemap_extent_info, start: u64, len: u64)
        -> Result<(), BchError>;
    /// @buf None for how many bytes the list would take.
    fn listxattr(dentry: Dentry<'_>, buf: Option<&mut [u8]>) -> Result<usize, BchError>
    {
        xattr::listxattr(dentry, buf)
    }
        => extern (dentry: *mut c::dentry, buf: *mut c_char, size: usize) -> isize
    {
        // SAFETY: the VFS's: a dentry of ours, and @size bytes at @buf, or NULL
        let r = unsafe { T::listxattr(Dentry::borrow_raw(dentry), opt_bytes_mut(buf.cast(), size)) };
        IntoRaw::into_raw(r)
    };
}

// ── bcachefs's ───────────────────────────────────────────────────────────

struct FileInodeOps;
struct SymlinkOps;
struct SpecialOps;

#[vtable]
impl InodeOps for FileInodeOps {
    fn fiemap(inode: &InodeInfo, info: &mut c::fiemap_extent_info, start: u64, len: u64)
        -> Result<(), BchError>
    {
        ret_to_result_void(unsafe { bch2_fiemap(inode.vfs().as_ptr(), info, start, len) })
    }
}

#[vtable]
impl InodeOps for SymlinkOps {
    /// The target, in the page cache: symlink() wrote it there.
    fn get_link(dentry: Option<Dentry<'_>>, inode: &InodeInfo, done: &mut c::delayed_call)
        -> *const c_char
    {
        let dentry = dentry.map_or(core::ptr::null_mut(), |d| d.as_ptr());
        unsafe { c::page_get_link(dentry, inode.vfs().as_ptr(), done).cast() }
    }
}

#[vtable]
impl InodeOps for SpecialOps {}

// i_op, as bch2_vfs_inode_init() sets it:
#[c_export]
pub static bch2_dir_inode_ops: c::inode_operations = InodeVTable::<DirOps>::OPS;
#[c_export]
pub static bch2_file_inode_ops: c::inode_operations = InodeVTable::<FileInodeOps>::OPS;
#[c_export]
pub static bch2_symlink_inode_ops: c::inode_operations = InodeVTable::<SymlinkOps>::OPS;
#[c_export]
pub static bch2_special_inode_ops: c::inode_operations = InodeVTable::<SpecialOps>::OPS;
