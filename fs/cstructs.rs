// SPDX-License-Identifier: GPL-2.0

//! The C interface defined in Rust, one module per header - see types/lib.rs.
//! So far, the bkey_ops methods Rust implements: see rust_c_extern!. And C's
//! x-macro lists, read from C for now: see xmacros.rs. And typed views of
//! C's tagged unions: see alloc/accounting_format.rs, data/extents_format.rs.

#[path = "alloc/accounting_format.rs"] pub mod alloc_accounting_format;
#[path = "data/extents_format.rs"] pub mod data_extents_format;
#[path = "fs/dirent_types.rs"] pub mod fs_dirent_types;
#[path = "fs/inode_types.rs"] pub mod fs_inode_types;
#[path = "fs/xattr_types.rs"] pub mod fs_xattr_types;
#[path = "xmacros.rs"] pub mod xmacros;

/// What crate::c re-exports from here: so far, the x-macro lists and the
/// tagged unions' views. A list that splices in another names it here:
/// crate::cstructs::c::SUB!.
pub mod c {
    #![allow(unused_imports)]

    pub use super::alloc_accounting_format::*;
    pub use super::data_extents_format::*;
    pub use super::xmacros::*;
}
