// SPDX-License-Identifier: GPL-2.0

//! POSIX ACLs on disk: an xattr of type POSIX_ACL_ACCESS or _DEFAULT, with an
//! empty name, its value a bch_acl_header and then the entries - each as long
//! as its tag says, a bch_acl_entry_short or, for ACL_USER and ACL_GROUP,
//! which carry an id, a bch_acl_entry.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{Le16, Le32};
use cstruct_macros::{c_const, tagged_union, CStruct, Plain, TypeInfo};

c_const! {
    pub const BCH_ACL_VERSION: u32 = 0x0001;
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_typedef]
pub struct bch_acl_entry {
    pub e_tag: Le16,
    pub e_perm: Le16,
    pub e_id: Le32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_typedef]
pub struct bch_acl_entry_short {
    pub e_tag: Le16,
    pub e_perm: Le16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_typedef]
pub struct bch_acl_header {
    pub a_version: Le32,
}

/* The tags are <uapi/linux/posix_acl.h>'s ACL_*, which C has from there: */
tagged_union! {
    pub union acl_entry in c::bch_acl_entry_short {
        tag e_tag: u16,
        arm user_obj:  c::bch_acl_entry_short = 0x01,
        arm user:      c::bch_acl_entry       = 0x02,
        arm group_obj: c::bch_acl_entry_short = 0x04,
        arm group:     c::bch_acl_entry       = 0x08,
        arm mask:      c::bch_acl_entry_short = 0x10,
        arm other:     c::bch_acl_entry_short = 0x20,
        size: arm,
    }
}
