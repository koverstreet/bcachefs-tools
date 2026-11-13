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
    // SAFETY: ops()'s, which only D's kobject has; sysfs's attribute and page
    unsafe {
        let obj = obj_of::<D>(kobj.cast());
        show_page(buf, |out| D::show(obj, &*attr, out))
    }
}

unsafe extern "C" fn store<D: Dir>(kobj: *mut c::kobject, attr: *mut c::attribute,
                                   buf: *const c_char, size: usize) -> isize {
    // SAFETY: ops()'s, which only D's kobject has
    let obj = unsafe { obj_of::<D>(kobj.cast()) };
    if let Err(e) = D::may_store(obj) {
        return e.class() as isize;
    }
    // SAFETY: sysfs's attribute, and buffer of @size bytes
    unsafe { store_buf(buf, size, |s| D::store(obj, &*attr, s)) }
}

/// A show: what @f writes - into sysfs's page @buf itself, cut off at its end -
/// or @f's error, as an errno.
///
/// # Safety
/// @buf is a PAGE_SIZE buffer.
unsafe fn show_page(buf: *mut c_char, f: impl FnOnce(&mut Printbuf) -> Result<(), BchError>) -> isize {
    // SAFETY: the caller's
    let mut out = unsafe { Printbuf::new_extern(buf, crate::os::kernel::PAGE_SIZE) };

    match f(&mut out) {
        Ok(()) => {
            out.ensure_trailing_newline();
            out.as_bytes().len() as isize
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
