// SPDX-License-Identifier: GPL-2.0

//! The VFS's side of vfs_ops! (os/linux_kernel/ops.rs): what its ops are
//! passed that's bcachefs's, as FromRaw impls.

use crate::os::ops::from_raw;
use crate::util::ffi::c_char;

use super::export::NameBuf;

from_raw! {
    /// get_name's, NAME_MAX + 1 bytes.
    &'a mut NameBuf       = *mut c_char         => |r| &mut *r.cast();
}
