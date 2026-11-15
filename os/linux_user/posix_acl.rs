// SPDX-License-Identifier: GPL-2.0

/// struct posix_acl, in userspace: nothing makes one there - the VFS does -
/// so there are none, and an Option<&PosixAcl> is always None.
pub enum PosixAcl {}
