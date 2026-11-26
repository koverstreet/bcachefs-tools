// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::alloc::accounting_format::DataType;
use crate::types::Le8;
use crate::util::vstructs::Head;
use cstruct_macros::{counted, whole, MaybeZeroable, Plain, TypeInfo};

#[counted(nr_devs)]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct ReplicasEntryV0 {
    pub data_type: Le8<DataType>,
    pub nr_devs: u8,
    pub devs: [u8; 0],
}

#[whole(tail = start: [u8], entries = thin ReplicasEntryV0)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct SbFieldReplicasV0 {
    pub start: [Head<ReplicasEntryV0>; 0],
}

#[counted(nr_devs)]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct ReplicasEntryV1 {
    pub data_type: Le8<DataType>,
    pub nr_devs: u8,
    pub nr_required: u8,
    /* No __counted_by in C: bch_replicas_cpu entries are all the size of the biggest entry */
    pub devs: [u8; 0],
}

#[whole(tail = start: [u8], entries = thin ReplicasEntryV1)]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct SbFieldReplicas {
    pub start: [Head<ReplicasEntryV1>; 0],
}

