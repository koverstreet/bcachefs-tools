// SPDX-License-Identifier: GPL-2.0

//! A mount's idmap, which maps the ids its users see to the filesystem's:
//! ownership and permission are as it maps the inode's ids.
//!
//! An id comes two ways: as the mount's users see it - vfsuid_t and
//! vfsgid_t, an iattr's - and as the inode's filesystem has it, kuid_t and
//! kgid_t. kuid() and kgid() take the first to the second.

use core::ffi::c_int;
use core::marker::PhantomData;
use core::ptr::read_volatile;

use crate::c;
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

    pub fn kuid(self, inode: &VfsInode, vfsuid: c::vfsuid_t) -> c::kuid_t {
        unsafe { c::from_vfsuid(self.0, inode.user_ns(), vfsuid) }
    }

    pub fn kgid(self, inode: &VfsInode, vfsgid: c::vfsgid_t) -> c::kgid_t {
        unsafe { c::from_vfsgid(self.0, inode.user_ns(), vfsgid) }
    }

    pub fn in_group_or_capable(self, inode: &VfsInode, gid: c::kgid_t) -> bool {
        unsafe {
            c::in_group_or_capable(self.0, inode.as_ptr(), c::make_vfsgid(self.0, inode.user_ns(), gid))
        }
    }
}
