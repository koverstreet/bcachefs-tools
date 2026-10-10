// SPDX-License-Identifier: GPL-2.0

//! The C interface defined in Rust, one module per header - see types/lib.rs.
//! So far, the bkey_ops methods Rust implements: see rust_c_extern!. And C's
//! x-macro lists, read from C for now: see xmacros.rs.

#[path = "fs/dirent_types.rs"] pub mod fs_dirent_types;
#[path = "fs/inode_types.rs"] pub mod fs_inode_types;
#[path = "fs/xattr_types.rs"] pub mod fs_xattr_types;
#[path = "xmacros.rs"] pub mod xmacros;

/// What crate::c re-exports from here: so far, the x-macro lists. A list
/// that splices in another names it here: crate::cstructs::c::SUB!.
pub mod c {
    #![allow(unused_imports)]

    pub use super::xmacros::*;
}
