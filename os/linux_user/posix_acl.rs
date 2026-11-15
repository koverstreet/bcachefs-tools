// SPDX-License-Identifier: GPL-2.0

#[path = "../posix_acl.rs"]
mod common;
pub use common::*;

/// struct posix_acl, in userspace: nothing makes one there - the VFS does -
/// so there are none, and an Option<&PosixAcl> is always None.
pub enum PosixAcl {}
