// SPDX-License-Identifier: GPL-2.0

//! The LSMs' hooks a filesystem calls itself.

use core::ffi::{c_int, c_void};
use core::ptr;

use crate::c;
use crate::errcode::{ret_to_c, ret_to_result_void, BchError};
use crate::os::vfs_inode::VfsInode;
use crate::util::ffi::bytes;
use crate::util::os_str::{cstr_name, OsStr};

/// The xattrs the LSMs label new @inode in @dir with - @name its name - each
/// a name and value to @set: security_inode_init_security().
pub fn inode_init_security<S, F>(inode: &VfsInode<S>, dir: &VfsInode<S>, name: Option<&c::qstr>, mut set: F)
    -> Result<(), BchError>
where
    F: FnMut(&OsStr, &[u8]) -> Result<(), BchError>,
{
    unsafe extern "C" fn initxattrs<F: FnMut(&OsStr, &[u8]) -> Result<(), BchError>>(
        _inode: *mut c::inode, xattrs: *const c::xattr, set: *mut c_void) -> c_int
    {
        let set = unsafe { &mut *set.cast::<F>() };
        // ended by one with no name
        let mut xattrs = (0..).map(|i| unsafe { &*xattrs.add(i) }).take_while(|x| !x.name.is_null());
        ret_to_c(xattrs.try_for_each(|x| unsafe { set(cstr_name(x.name), bytes(x.value, x.value_len)) }))
    }

    ret_to_result_void(unsafe {
        c::security_inode_init_security(inode.as_ptr(), dir.as_ptr(), name.map_or(ptr::null(), ptr::from_ref),
                                        Some(initxattrs::<F>), (&raw mut set).cast())
    })
}
