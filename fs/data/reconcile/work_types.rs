// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::c_uint;
use cstruct_macros::{CStruct, TypeInfo, c_enum, c_typedef, c_xmacro, tagged_union};

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

c_enum! {
    #[closed]
    pub enum reconcile_scan_type: u32 {
        RECONCILE_SCAN_TYPES(t) => RECONCILE_SCAN_ ## t,
    }
}

c_typedef! {
    pub type reconcile_scan_device = c_uint;
}

c_typedef! {
    pub type reconcile_scan_inum = u64;
}

tagged_union! {
    #[derive(Clone, Copy, CStruct)]*
    /// A scan reconcile's asked for - of what, and for a device's or an
    /// inode's, which.
    #[derive(TypeInfo)]
    pub struct reconcile_scan {
        tag type_: enum c::reconcile_scan_type,
        arms from RECONCILE_SCAN_TYPES(x) => x: reconcile_scan_ ## x = c::reconcile_scan_type::RECONCILE_SCAN_ ## x,
        pad: [u64; 2],
        size: pad,
        plain,

        #[repr(C)]
        pub struct reconcile_scan_fs {},
        #[repr(C)]
        pub struct reconcile_scan_metadata {},
        #[repr(C)]
        pub struct reconcile_scan_pending {},
        #[repr(C)]
        pub struct reconcile_scan_stripes {},
    }
}

