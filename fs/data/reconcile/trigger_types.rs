// SPDX-License-Identifier: GPL-2.0

//! The data types of data/reconcile/trigger_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{c_enum, CStruct};

c_enum! {
    #[closed]
    pub enum set_needs_reconcile_ctx: u32 {
        SET_NEEDS_RECONCILE_opt_change,
        SET_NEEDS_RECONCILE_opt_change_indirect,
        SET_NEEDS_RECONCILE_foreground,
        SET_NEEDS_RECONCILE_other,
    }
}

/* Inodes in different snapshots may have different IO options: */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct snapshot_io_opts_entry {
    pub snapshot: u32,
    pub io_opts: c::bch_inode_opts,
}
c_default!(snapshot_io_opts_entry);

#[repr(C)]
#[derive(CStruct)]
pub struct per_snapshot_io_opts {
    pub cur_inum: u64,
    pub metadata: bool,
    pub fs_scan_cookie: bool,
    pub inum_scan_cookie: bool,
    pub dev_cookie: c::bch_devs_mask,

    pub fs_io_opts: c::bch_inode_opts,
    #[c("DARRAY(struct snapshot_io_opts_entry) d")]
    pub d: DArray<c::snapshot_io_opts_entry>,
}
c_default!(per_snapshot_io_opts);
