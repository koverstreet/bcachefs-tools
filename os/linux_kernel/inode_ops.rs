// SPDX-License-Identifier: GPL-2.0

//! An inode's operations as a trait, InodeOps, declared by vfs_ops!
//! (ops.rs), as the super ops are.
//!
//! A filesystem has a table per kind of inode - a directory's, a regular
//! file's - each a zero-sized type implementing it, with no state: each op is
//! an associated fn. The ops every table has - getattr, setattr, listxattr,
//! the ACLs, fileattr - are the inode's own, InodeAttrs: provided, their
//! HAS_* true unless an impl says otherwise; the rest are each table's.

use core::ffi::CStr;
use core::mem::ManuallyDrop;

use crate::c;
use crate::errcode::{BchError, Errcode};
use crate::os::dentry::Dentry;
use crate::os::fiemap::Fiemap;
use crate::os::file::OpeningTmpfile;
use crate::os::idmap::MntIdmap;
use crate::posix::mode::umode_t;
use crate::os::ops::IntoRaw;
use crate::os::path::Path;
use crate::os::posix_acl::{AclType, PosixAcl};
use crate::os::super_block::IsInode;
use crate::util::ffi::{c_char, opt_bytes_mut};

/// What an inode does whichever of its type's tables it has: InodeOps'
/// provided ops.
pub trait InodeAttrs: IsInode + Sized {
    fn getattr(&self, idmap: MntIdmap<'_>, stat: &mut c::kstat, request_mask: u32);
    fn setattr(&self, idmap: MntIdmap<'_>, dentry: Dentry<'_, Self>, attr: &mut c::iattr)
        -> Result<(), BchError>;
    fn get_acl(&self, ty: AclType) -> Result<Option<PosixAcl>, BchError>;
    fn set_acl(&self, idmap: MntIdmap<'_>, acl: Option<&PosixAcl>, ty: AclType) -> Result<(), BchError>;
    fn fileattr_get(&self, fa: &mut c::file_kattr);
    fn fileattr_set(&self, dentry: Dentry<'_, Self>, fa: &mut c::file_kattr) -> Result<(), BchError>;
    fn listxattr(dentry: Dentry<'_, Self>, buf: Option<&mut [u8]>) -> Result<usize, BchError>;
}

crate::vfs_ops! {
    pub(crate) trait InodeOps for c::inode_operations, InodeVTable<T: InodeOps> = T;
    type Inode: InodeAttrs;

    /// What d_splice_alias() makes of the inode @dentry names.
    fn lookup(dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>, flags: u32) -> *mut c::dentry;
    #[cfg(not(linux_ge = "7.3"))]
    fn create(idmap: MntIdmap<'_>, dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>, mode: umode_t,
                  excl: bool) -> Result<(), BchError>;
    #[cfg(linux_ge = "7.3")]
    fn create(idmap: MntIdmap<'_>, dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>, mode: umode_t)
        -> Result<(), BchError>;
    fn link(old: Dentry<'_, Self::Inode>, dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>)
        -> Result<(), BchError>;
    fn unlink(dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>) -> Result<(), BchError>;
    fn symlink(idmap: MntIdmap<'_>, dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>, target: &CStr)
        -> Result<(), BchError>;
    /// NULL for the dentry it was given, instantiated.
    fn mkdir(idmap: MntIdmap<'_>, dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>, mode: umode_t)
        -> Result<(), BchError>;
    fn rmdir(dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>) -> Result<(), BchError>;
    fn mknod(idmap: MntIdmap<'_>, dir: &Self::Inode, dentry: Dentry<'_, Self::Inode>, mode: umode_t,
                 rdev: c::dev_t) -> Result<(), BchError>;
    fn rename(idmap: MntIdmap<'_>, src_dir: &Self::Inode, src: Dentry<'_, Self::Inode>,
                  dst_dir: &Self::Inode, dst: Dentry<'_, Self::Inode>, flags: u32) -> Result<(), BchError>;
    fn tmpfile(idmap: MntIdmap<'_>, dir: &Self::Inode, file: OpeningTmpfile<'_, Self::Inode>, mode: umode_t)
        -> Result<(), BchError>;

    fn getattr(idmap: MntIdmap<'_>, path: &Path<Self::Inode>, stat: &mut c::kstat, request_mask: u32,
                   _query_flags: u32) -> Result<(), BchError>
    {
        path.dentry().inode().getattr(idmap, stat, request_mask);
        Ok(())
    };
    fn setattr(idmap: MntIdmap<'_>, dentry: Dentry<'_, Self::Inode>, attr: &mut c::iattr)
        -> Result<(), BchError>
    {
        dentry.inode().setattr(idmap, dentry, attr)
    };
    /// An RCU walk can't sleep, as a lookup may: ECHILD, to try again
    /// without.
    fn get_inode_acl(inode: &Self::Inode, ty: AclType, rcu: bool)
        -> Result<Option<PosixAcl>, BchError>
    {
        if rcu {
            return Err(Errcode::ECHILD_get_inode_acl_rcu.into());
        }
        inode.get_acl(ty)
    };
    fn set_acl(idmap: MntIdmap<'_>, dentry: Dentry<'_, Self::Inode>, acl: Option<ManuallyDrop<PosixAcl>>,
                   ty: AclType) -> Result<(), BchError>
    {
        dentry.inode().set_acl(idmap, acl.as_deref(), ty)
    };
    fn fileattr_get(dentry: Dentry<'_, Self::Inode>, fa: &mut c::file_kattr) -> Result<(), BchError>
    {
        dentry.inode().fileattr_get(fa);
        Ok(())
    };
    fn fileattr_set(_idmap: MntIdmap<'_>, dentry: Dentry<'_, Self::Inode>, fa: &mut c::file_kattr)
        -> Result<(), BchError>
    {
        dentry.inode().fileattr_set(dentry, fa)
    };

    /// @dentry None in an RCU walk.
    fn get_link(dentry: Option<Dentry<'_, Self::Inode>>, inode: &Self::Inode, done: &mut c::delayed_call)
        -> *const c_char;
    fn fiemap(inode: &Self::Inode, info: Fiemap<'_>, start: u64, len: u64) -> Result<(), BchError>;
    /// @buf None for how many bytes the list would take.
    fn listxattr(dentry: Dentry<'_, Self::Inode>, buf: Option<&mut [u8]>) -> Result<usize, BchError>
    {
        <Self::Inode as InodeAttrs>::listxattr(dentry, buf)
    }
        => extern (dentry: *mut c::dentry, buf: *mut c_char, size: usize) -> isize
    {
        // SAFETY: the VFS's: a dentry of ours, and @size bytes at @buf, or NULL
        let r = unsafe { T::listxattr(Dentry::borrow_raw(dentry), opt_bytes_mut(buf.cast(), size)) };
        IntoRaw::into_raw(r)
    };
}
