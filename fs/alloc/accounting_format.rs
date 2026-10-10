// SPDX-License-Identifier: GPL-2.0
//! alloc/accounting_format.h's disk_accounting_pos, as Rust reads it: a
//! tagged_union! over the storage bindgen makes of C's definition, its arms
//! C's bch_acct_<type>s from BCH_DISK_ACCOUNTING_TYPES() - see
//! cstruct-macros/src/tagged_union.rs. When the types are Rust's, this
//! declaration defines it.

use crate::c;
use cstruct_macros::tagged_union;

tagged_union! {
    pub struct disk_accounting_pos in c::disk_accounting_pos {
        tag type_: u8 = c::disk_accounting_type,
        arms from BCH_DISK_ACCOUNTING_TYPES(f, nr, ..) => f: c::bch_acct_ ## f = nr,
        pad: c::bpos,
        packed,
    }
}
