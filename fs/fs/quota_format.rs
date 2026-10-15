// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{Le32, Le64};
use crate::util::vstructs::{vstruct, Head};
use cstruct_macros::{c_enum, whole, CStruct, Plain, TypeInfo};

/* KEY_TYPE_quota: */
c_enum! {
    #[open]
    pub enum quota_types: u32 {
        QTYP_USR = 0,
        QTYP_GRP = 1,
        QTYP_PRJ = 2,
        QTYP_NR = 3,
    }
}

c_enum! {
    #[open]
    pub enum quota_counters: u32 {
        Q_SPC = 0,
        Q_INO = 1,
        Q_COUNTERS = 2,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_quota_counter {
    pub hardlimit: Le64,
    pub softlimit: Le64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
#[c_name(bch_quota)]
pub struct Quota {
    pub v: c::bch_val,
    pub c: [c::bch_quota_counter; c::Q_COUNTERS as usize],
}

/* BCH_SB_FIELD_quota: */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_quota_counter {
    pub timelimit: Le32,
    pub warnlimit: Le32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_quota_type {
    pub flags: Le64,
    pub c: [c::bch_sb_quota_counter; c::Q_COUNTERS as usize],
}

#[whole]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_quota {
    pub field: Head<c::bch_sb_field>,
    pub q: [c::bch_sb_quota_type; c::QTYP_NR as usize],
}
vstruct!(bch_sb_field_quota, field.u64s, start = 0);
