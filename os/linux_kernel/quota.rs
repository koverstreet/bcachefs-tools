// SPDX-License-Identifier: GPL-2.0

//! Quota ids as the quota subsystem has them: a kqid, a type and an id in a
//! user namespace. On disk, an id is the initial namespace's.

use crate::c;
use crate::util::ffi::c_char;

/// kqid is kernel::bindings', so its methods are a trait's.
pub trait Kqid: Sized {
    /// @ty's quota @id, in the initial user namespace.
    fn new(ty: c::quota_type, id: u32) -> Self;

    /// @ty's quota @id, in the calling task's user namespace.
    fn new_current(ty: c::quota_type, id: u32) -> Self;

    /// Its id in the initial user namespace.
    fn id(self) -> u32;

    /// Its owner warned of @msg, about device @dev: quota_send_warning().
    fn send_warning(self, dev: c::dev_t, msg: c_char);
}

impl Kqid for c::kqid {
    fn new(ty: c::quota_type, id: u32) -> Self {
        unsafe { c::make_kqid(&raw mut c::init_user_ns, ty, id) }
    }

    fn new_current(ty: c::quota_type, id: u32) -> Self {
        unsafe { c::make_kqid(c::current_user_ns(), ty, id) }
    }

    fn id(self) -> u32 {
        unsafe { c::from_kqid(&raw mut c::init_user_ns, self) }
    }

    fn send_warning(self, dev: c::dev_t, msg: c_char) {
        unsafe { c::quota_send_warning(self, dev, msg) }
    }
}
