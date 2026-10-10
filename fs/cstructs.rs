// SPDX-License-Identifier: GPL-2.0

//! The C interface defined in Rust, one module per header - see types/lib.rs.
//! So far, the bkey_ops methods Rust implements: see rust_c_extern!.

#[path = "fs/dirent_types.rs"] pub mod fs_dirent_types;
#[path = "fs/inode_types.rs"] pub mod fs_inode_types;
#[path = "fs/xattr_types.rs"] pub mod fs_xattr_types;
