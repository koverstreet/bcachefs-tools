// SPDX-License-Identifier: GPL-2.0

//! POSIX ACLs on disk: an xattr of type POSIX_ACL_ACCESS or _DEFAULT, with an
//! empty name, its value a AclHeader and then the entries - each as long
//! as its tag says, a AclEntryShort or, for ACL_USER and ACL_GROUP,
//! which carry an id, a bch_acl_entry.

#![allow(non_camel_case_types)]

use crate::types::{Le16, Le32};
use cstruct_macros::{tagged_union, MaybeZeroable, Plain, TypeInfo};

pub const BCH_ACL_VERSION: u32 = 0x0001;

#[repr(C, packed)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_acl_entry {
    pub e_tag: Le16,
    pub e_perm: Le16,
    pub e_id: Le32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct AclEntryShort {
    pub e_tag: Le16,
    pub e_perm: Le16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct AclHeader {
    pub a_version: Le32,
}

/* The tags are <uapi/linux/posix_acl.h>'s ACL_*, which C has from there: */
tagged_union! {
    in crate::posix::acl_format;
    #[storage(crate::posix::acl_format::AclEntryShort)]
    #[tag(e_tag: u16)]
    #[size(arm)]
    pub enum acl_entry {
        user_obj(crate::posix::acl_format::AclEntryShort)  = 0x01,
        user(crate::posix::acl_format::bch_acl_entry)            = 0x02,
        group_obj(crate::posix::acl_format::AclEntryShort) = 0x04,
        group(crate::posix::acl_format::bch_acl_entry)           = 0x08,
        mask(crate::posix::acl_format::AclEntryShort)      = 0x10,
        other(crate::posix::acl_format::AclEntryShort)     = 0x20,
    }
}
