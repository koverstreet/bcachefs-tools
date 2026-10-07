// SPDX-License-Identifier: GPL-2.0

//! The data types of sb/io_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_enum, CStruct};
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_sb_field_ops {
    pub validate: Option<unsafe extern "C" fn(*mut c::bch_sb, *mut c::bch_sb_field,
                                              c::bch_validate_flags, *mut c::printbuf) -> core::ffi::c_int>,
    pub to_text: Option<unsafe extern "C" fn(*mut c::printbuf, *mut c::bch_fs, *mut c::bch_sb, *mut c::bch_sb_field)>,
}

/*
 * bringup: this write is part of bringing the filesystem up - allowed before
 * a start has begun, see __bch2_write_super()
 */
c_enum! {
    #[flags]
    pub enum bch_sb_write_flags: u32 {
        BCH_SB_WRITE_bringup = 1 << 0,
    }
}

/*
 * Permission to modify a superblock field, and the thing that writes it back.
 * Hold sb_lock - guard(mutex_noio)(&c->sb_lock) - and declare one of these
 * under it. Declaration order is load-bearing: declared after the lock guard,
 * this destructs first, so the write happens while sb_lock is still held.
 *
 * The setters return whether the fact was NEW, which is the caller's business
 * (an fsck message unsuppresses on novelty). The write-back accumulates
 * separately, so forgetting to use that return can't lose a superblock write.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct sb_write {
    pub c: *mut c::bch_fs,
    pub dirty: bool,
    pub replicas: bool,
    pub flags: c::bch_sb_write_flags,
}
c_default!(sb_write);
