// SPDX-License-Identifier: GPL-2.0

//! sysfs files: os/sysfs.rs's, through the kernel's C structs - a struct
//! attribute each, and a Dir's sysfs_ops.
//!
//! A show writes to sysfs's page, cut at PAGE_SIZE - 1 - ENOMEM if the
//! printbuf couldn't allocate.

use core::ffi::CStr;

use crate::c;
use crate::errcode::BchError;
use crate::util::ffi::c_char;
use crate::util::Printbuf;

#[path = "../sysfs.rs"]
mod common;
pub use common::*;

pub type Attribute = c::attribute;

pub const fn attribute(name: &'static CStr, mode: u16) -> Attribute {
    // the rest of a struct attribute is lockdep's, zeroed for a static one
    let mut attr: c::attribute = ::pin_init::zeroed();
    attr.name = name.as_ptr().cast();
    attr.mode = mode;
    attr
}

/// A Dir's sysfs_ops.
pub(crate) const fn ops<D: Dir>() -> c::sysfs_ops {
    c::sysfs_ops {
        show:  Some(show::<D>),
        store: Some(store::<D>),
    }
}

unsafe extern "C" fn show<D: Dir>(kobj: *mut c::kobject, attr: *mut c::attribute,
                                  buf: *mut c_char) -> isize {
    // ops()'s, which only D's kobject has, its files Attr<D::Obj>s; sysfs's
    // attribute and page
    unsafe {
        let (obj, file) = (obj_of::<D>(kobj.cast()), Attr::<D::Obj>::of(&*attr));
        show_page(buf, |out| file.show(obj, out))
    }
}

unsafe extern "C" fn store<D: Dir>(kobj: *mut c::kobject, attr: *mut c::attribute,
                                   buf: *const c_char, size: usize) -> isize {
    // as show()'s; a buffer of @size bytes
    unsafe {
        let (obj, file) = (obj_of::<D>(kobj.cast()), Attr::<D::Obj>::of(&*attr));
        store_buf(buf, size, |s| file.store(obj, s))
    }
}

/// A show: what @f writes, copied to sysfs's page @buf - cut off at its end,
/// a nul's room left, as sysfs wants - or @f's error, as an errno.
///
/// # Safety
/// @buf is a PAGE_SIZE buffer.
unsafe fn show_page(buf: *mut c_char, f: impl FnOnce(&mut Printbuf) -> Result<(), BchError>) -> isize {
    let mut out = Printbuf::new();

    match f(&mut out) {
        Ok(()) => {
            out.ensure_trailing_newline();
            let text = out.as_bytes();
            let n = text.len().min(crate::os::kernel::PAGE_SIZE - 1);
            // SAFETY: the caller's
            unsafe { core::ptr::copy_nonoverlapping(text.as_ptr(), buf.cast::<u8>(), n) };
            n as isize
        }
        Err(e) => e.class() as isize,
    }
}

/// A store: @f given what was written, @size bytes at @buf - all of it taken
/// if @f succeeds. Not UTF-8 is EINVAL.
///
/// # Safety
/// @buf has @size bytes.
unsafe fn store_buf(buf: *const c_char, size: usize,
                    f: impl FnOnce(&str) -> Result<(), BchError>) -> isize {
    // SAFETY: the caller's
    let bytes = unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), size) };
    let ret = core::str::from_utf8(bytes)
        .map_err(|_| BchError::from(c::EINVAL))
        .and_then(f);

    match ret {
        Ok(())  => size as isize,
        Err(e)  => e.class() as isize,
    }
}
