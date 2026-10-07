// SPDX-License-Identifier: GPL-2.0

//! The data types of data/reconcile/work_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_const, c_enum, c_extern, c_xmacro};
use nestify::nest;

c_xmacro! {
    RECONCILE_SCAN_TYPES(x) {
        (fs),
        (metadata),
        (pending),
        (stripes),
        (device),
        (inum),
    }
}

macro_rules! __reconcile_scan_type_0 {
    ([$($acc:tt)*] $(($t:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum reconcile_scan_type: u32 {
                $($acc)*
                $([<RECONCILE_SCAN_ $t>],)*
            }
        }
    } };
}
RECONCILE_SCAN_TYPES!(__reconcile_scan_type_0 []);

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]*
    pub struct reconcile_scan {
        pub type_: c::reconcile_scan_type,

        #[c_anon]
        pub id: pub union reconcile_scan_id {
            pub dev: core::ffi::c_uint,
            pub inum: u64,
        },
    }
}
c_default!(reconcile_scan);

c_const! {
    /* No opt change touches more than one bracketed reconcile scan today: */
    #[c_int]
    pub const BCH_OPT_CHANGE_SCANS_MAX: u32 = 4;
}

/*
 * The reconcile-scan cookies an opt change registered as in-flight - see
 * bch2_set_reconcile_needs_scan_pre(). Constructed empty, populated by
 * bch2_opt_hook_pre_set(); the destructor unregisters them, so an opt change
 * that errors out (or never reaches bch2_opt_hook_post_set()) doesn't leak a
 * registration and wedge the reconcile thread on that cookie.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct opt_change_scope {
    pub c: *mut c::bch_fs,
    pub nr: core::ffi::c_uint,
    pub cookies: [u64; c::BCH_OPT_CHANGE_SCANS_MAX as usize],
}
c_default!(opt_change_scope);

// What Rust calls of data/reconcile/work.h: C gets these as prototypes, in data/reconcile/work_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_opt_change_scope_exit(arg1: *mut c::opt_change_scope);
}
