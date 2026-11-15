// SPDX-License-Identifier: GPL-2.0

//! sysfs files, for userspace: os/sysfs.rs's, in the tree http.rs serves
//! (kobject.rs).

use core::ffi::CStr;

use crate::errcode::BchError;
use crate::os::kobject::Kobject;
use crate::util::Printbuf;

#[path = "../sysfs.rs"]
mod common;
pub use common::*;

pub struct Attribute {
    pub(crate) name: &'static CStr,
    pub(crate) mode: u16,
}

pub const fn attribute(name: &'static CStr, mode: u16) -> Attribute {
    Attribute { name, mode }
}

/// Reading a file of a Dir's kobject.
pub(crate) type ShowFn = unsafe fn(&Kobject, &Attribute, &mut Printbuf) -> Result<(), BchError>;

/// # Safety
/// @kobj is a D's, and @attr one of its groups' files.
pub(crate) unsafe fn show<D: Dir>(kobj: &Kobject, attr: &Attribute, out: &mut Printbuf) -> Result<(), BchError> {
    let (obj, file) = unsafe { (obj_of::<D>(core::ptr::from_ref(kobj).cast_mut()), Attr::<D::Obj>::of(attr)) };
    file.show(obj, out)
}

/// Writing a file of a Dir's kobject.
pub(crate) type StoreFn = unsafe fn(&Kobject, &Attribute, &str) -> Result<(), BchError>;

/// # Safety
/// As show()'s.
pub(crate) unsafe fn store<D: Dir>(kobj: &Kobject, attr: &Attribute, s: &str) -> Result<(), BchError> {
    let (obj, file) = unsafe { (obj_of::<D>(core::ptr::from_ref(kobj).cast_mut()), Attr::<D::Obj>::of(attr)) };
    file.store(obj, s)
}
