// SPDX-License-Identifier: GPL-2.0

//! The VFS's xattr handlers: sb->s_xattr, a NULL-terminated table of them,
//! each a namespace's ops - an XattrHandler, dispatched by its prefix.

use core::ffi::{c_int, c_void, CStr};

use crate::c;
use crate::errcode::{ret_to_class, BchError};
use crate::os::dentry::Dentry;
use crate::os::idmap::RawIdmap;
use crate::os::ops::FromRaw;
use crate::os::posix_acl::AclType;
use crate::os::super_block::IsInode;
use crate::types::CStatic;
use crate::util::ffi::{c_char, opt_bytes, opt_bytes_mut};

/// The namespace a handler is dispatched for.
pub(crate) trait XattrPrefix {
    const PREFIX: &'static CStr;
}

/// A getxattr result as C's int: the length, or the error's class.
fn len_to_c(r: Result<usize, BchError>) -> c_int {
    match r {
        Ok(len) => len as c_int,
        Err(e)  => e.class(),
    }
}

crate::vfs_ops! {
    /// An xattr namespace's ops: an entry of sb->s_xattr.
    pub(crate) trait XattrHandler: XattrPrefix for c::xattr_handler,
        XattrVTable<T: XattrHandler> = T, |h| { h.prefix = T::PREFIX.as_ptr().cast(); };
    type Inode: IsInode;

    /// Whether @dentry's caller sees the namespace's xattrs listed.
    fn list(dentry: Dentry<'_, Self::Inode>) -> bool;
    /// Xattr @name's value into @buffer, or with None, how long it is.
    fn get(inode: &Self::Inode, name: &CStr, buffer: Option<&mut [u8]>) -> Result<usize, BchError>
        => extern (_handler: *const c::xattr_handler, _dentry: *mut c::dentry, inode: *mut c::inode,
                   name: *const c_char, buffer: *mut c_void, size: usize) -> c_int
    {
        len_to_c(unsafe { T::get(FromRaw::from_raw(inode), FromRaw::from_raw(name), opt_bytes_mut(buffer, size)) })
    };
    /// Xattr @name set to @value - with None, removed - as setxattr(2)'s
    /// @flags say.
    fn set(dentry: Dentry<'_, Self::Inode>, inode: &Self::Inode, name: &CStr, value: Option<&[u8]>,
           flags: c_int) -> Result<(), BchError>
        => extern (_handler: *const c::xattr_handler, _idmap: RawIdmap, dentry: *mut c::dentry,
                   inode: *mut c::inode, name: *const c_char, value: *const c_void, size: usize,
                   flags: c_int) -> c_int
    {
        ret_to_class(unsafe {
            T::set(FromRaw::from_raw(dentry), FromRaw::from_raw(inode), FromRaw::from_raw(name),
                   opt_bytes(value, size), flags)
        })
    };
}

/// sb->s_xattr: N handlers, then the NULL that ends the table.
#[repr(C)]
struct Table<const N: usize> {
    handlers: [*const c::xattr_handler; N],
    null:     *const c::xattr_handler,
}

pub struct XattrHandlers<const N: usize>(CStatic<Table<N>>);

impl<const N: usize> XattrHandlers<N> {
    pub const fn new(handlers: [&'static CStatic<c::xattr_handler>; N]) -> Self {
        let mut table = Table { handlers: [core::ptr::null(); N], null: core::ptr::null() };
        let mut i = 0;
        while i < N {
            table.handlers[i] = core::ptr::from_ref(handlers[i]).cast();
            i += 1;
        }
        // pointers to statics, and NULL
        XattrHandlers(unsafe { CStatic::new(table) })
    }

    pub(crate) fn as_ptr(&self) -> *const *const c::xattr_handler {
        core::ptr::from_ref(&*self.0).cast()
    }
}

/// What a POSIX ACL's xattr lists under for @dentry's caller - None if it
/// doesn't: the kernel's handler's, xattr_handler_can_list() then
/// xattr_prefix().
pub(crate) fn posix_acl_list_prefix<I: IsInode>(ty: AclType, dentry: Dentry<'_, I>) -> Option<&'static CStr> {
    // the kernel's, and immutable
    let handler: &'static c::xattr_handler = unsafe {
        match ty {
            AclType::Access  => &c::nop_posix_acl_access,
            AclType::Default => &c::nop_posix_acl_default,
        }
    };

    if let Some(list) = handler.list {
        if !unsafe { list(dentry.as_ptr()) } {
            return None;
        }
    }

    // the handlers' strings are static
    let p = if handler.prefix.is_null() { handler.name } else { handler.prefix };
    Some(unsafe { CStr::from_ptr(p.cast()) })
}
