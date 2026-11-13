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
pub(crate) type ShowFn = fn(&Kobject, &Attribute, &mut Printbuf) -> Result<(), BchError>;

pub(crate) fn show<D: Dir>(kobj: &Kobject, attr: &Attribute, out: &mut Printbuf) -> Result<(), BchError> {
    let obj = unsafe { obj_of::<D>(core::ptr::from_ref(kobj).cast_mut()) };
    D::show(obj, attr, out)
}

/// Writing a file of a Dir's kobject.
pub(crate) type StoreFn = fn(&Kobject, &Attribute, &str) -> Result<(), BchError>;

pub(crate) fn store<D: Dir>(kobj: &Kobject, attr: &Attribute, s: &str) -> Result<(), BchError> {
    let obj = unsafe { obj_of::<D>(core::ptr::from_ref(kobj).cast_mut()) };
    D::may_store(obj)?;
    D::store(obj, attr, s)
}
