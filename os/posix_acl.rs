// SPDX-License-Identifier: GPL-2.0

/// Which of an inode's ACLs: its access ACL, or a directory's default ACL,
/// which what's made in it inherits. The values are the VFS's ACL_TYPE_*.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum AclType {
    Access  = 0x8000,
    Default = 0x4000,
}

impl AclType {
    pub const ALL: [Self; 2] = [Self::Access, Self::Default];
}
