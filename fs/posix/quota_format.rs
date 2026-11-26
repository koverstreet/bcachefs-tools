// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::format::bch_val;
use crate::types::{Le32, Le64};
use cstruct_macros::{bch_enum, whole, MaybeZeroable, Plain, TypeInfo, SizedVal};

/* KEY_TYPE_quota: */
#[bch_enum(open)]
#[nr]
#[repr(u32)]
pub enum QuotaType {
    QTYP_USR = 0,
    QTYP_GRP = 1,
    QTYP_PRJ = 2,
}

#[bch_enum(open)]
#[nr]
#[repr(u32)]
pub enum QuotaCounter {
    Q_SPC = 0,
    Q_INO = 1,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_quota_counter {
    pub hardlimit: Le64,
    pub softlimit: Le64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo, SizedVal)]
pub struct Quota {
    pub v: bch_val,
    pub c: [bch_quota_counter; QuotaCounter::NR as usize],
}

/* BCH_SB_FIELD_quota: */
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_quota_counter {
    pub timelimit: Le32,
    pub warnlimit: Le32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_quota_type {
    pub flags: Le64,
    pub c: [bch_sb_quota_counter; QuotaCounter::NR as usize],
}

#[whole]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_field_quota {
    pub q: [bch_sb_quota_type; QuotaType::NR as usize],
}
