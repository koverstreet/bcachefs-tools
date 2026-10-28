// SPDX-License-Identifier: GPL-2.0

//! A mount's idmap, which maps the ids its users see to the filesystem's:
//! ownership and permission are as it maps the inode's ids.
//!
//! An id comes two ways: as the mount's users see it - vfsuid_t and
//! vfsgid_t, an iattr's - and as the inode's filesystem has it, kuid_t and
//! kgid_t. kuid() and kgid() take the first to the second.

use core::ffi::c_int;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::read_volatile;

use crate::acl::PosixAcl;
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::util::file::File;
use crate::vfs::vfs_inode::VfsInode;

/// The idmap of a mount, for 'f: the file opened through it is borrowed - an
/// open file pins its mount - or the VFS call it was passed to runs.
#[derive(Clone, Copy)]
pub struct MntIdmap<'f>(*mut c::mnt_idmap, PhantomData<&'f c::mnt_idmap>);

impl<'f> MntIdmap<'f> {
    /// # Safety
    /// @idmap is the mount's, live for 'f.
    pub unsafe fn borrow_raw(idmap: *mut c::mnt_idmap) -> Self {
        MntIdmap(idmap, PhantomData)
    }

    pub fn of(file: &'f File) -> Self {
        let file = file.as_ptr();
        // SAFETY: an open file's mount. READ_ONCE(), as mnt_idmap(): the
        // idmap is published by do_idmap_mount()'s smp_store_release()
        let idmap = unsafe {
            // 6.18 made f_path a const union member, __f_path its writable alias
            #[cfg(linux_ge = "6.18")]
            let mnt = (*file).__bindgen_anon_1.f_path.mnt;
            #[cfg(not(linux_ge = "6.18"))]
            let mnt = (*file).f_path.mnt;
            read_volatile(&raw const (*mnt).mnt_idmap)
        };
        MntIdmap(idmap, PhantomData)
    }

    pub fn owner_or_capable(self, inode: &VfsInode) -> bool {
        unsafe { c::inode_owner_or_capable(self.0, inode.as_ptr()) }
    }

    /// Whether the caller has @mask - MAY_* - on @inode, by the whole VFS
    /// stack: ACLs and LSMs too.
    pub fn permission(self, inode: &VfsInode, mask: u32) -> bool {
        unsafe { c::inode_permission(self.0, inode.as_ptr(), mask as c_int) == 0 }
    }

    /// @inode's owner as the mount's users see it, as stat(2) has it: a
    /// vfsuid_t and vfsgid_t, in the kuid_t and kgid_t they're carried as.
    pub fn owner(self, inode: &VfsInode) -> (c::kuid_t, c::kgid_t) {
        let uid = unsafe { c::make_vfsuid(self.0, inode.user_ns(), inode.kuid()) };
        let gid = unsafe { c::make_vfsgid(self.0, inode.user_ns(), inode.kgid()) };
        (c::kuid_t { val: uid.val }, c::kgid_t { val: gid.val })
    }

    pub fn kuid(self, inode: &VfsInode, vfsuid: c::vfsuid_t) -> c::kuid_t {
        unsafe { c::from_vfsuid(self.0, inode.user_ns(), vfsuid) }
    }

    pub fn kgid(self, inode: &VfsInode, vfsgid: c::vfsgid_t) -> c::kgid_t {
        unsafe { c::from_vfsgid(self.0, inode.user_ns(), vfsgid) }
    }

    /// The mode @inode takes with @acl as its access ACL, into @mode: the
    /// ACL's permissions, and setgid as setattr_copy() keeps it - and @acl
    /// None if the mode says all it would.
    pub fn acl_update_mode(self, inode: &VfsInode, mode: &mut c::umode_t,
                                  acl: &mut Option<ManuallyDrop<PosixAcl>>) -> Result<(), BchError> {
        let mut raw = acl.as_deref().map_or(core::ptr::null_mut(), PosixAcl::as_raw);
        ret_to_result(unsafe { c::posix_acl_update_mode(self.0, inode.as_ptr(), mode, &mut raw) })?;
        if raw.is_null() {
            *acl = None;
        }
        Ok(())
    }

    pub fn in_group_or_capable(self, inode: &VfsInode, gid: c::kgid_t) -> bool {
        unsafe {
            c::in_group_or_capable(self.0, inode.as_ptr(), c::make_vfsgid(self.0, inode.user_ns(), gid))
        }
    }
}
