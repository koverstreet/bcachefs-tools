// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{c_enum, c_extern, CStruct};

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
#[derive(Clone, Copy, Default, CStruct)]
pub struct snapshot_io_opts_entry {
    pub snapshot: u32,
    pub io_opts: c::bch_inode_opts,
}

#[repr(C)]
#[derive(Default, CStruct)]
pub struct per_snapshot_io_opts {
    pub cur_inum: u64,
    pub metadata: bool,
    pub fs_scan_cookie: bool,
    pub inum_scan_cookie: bool,
    pub dev_cookie: c::bch_devs_mask,

    pub fs_io_opts: c::bch_inode_opts,
    pub d: DArray<c::snapshot_io_opts_entry>,
}

c_extern! {
    pub fn bch2_bkey_get_io_opts(trans: *mut c::btree_trans, snapshot_io_opts: *mut c::per_snapshot_io_opts,
                                 k: c::bkey_s_c, opts: *mut c::bch_inode_opts) -> c_int;
    pub fn bch2_bkey_set_needs_reconcile(trans: *mut c::btree_trans,
                                         snapshot_io_opts: *mut c::per_snapshot_io_opts,
                                         opts: *mut c::bch_inode_opts, k: c::bkey_s, buf_u64s: c_uint,
                                         ctx: c::set_needs_reconcile_ctx, flags: u32) -> c_int;
    pub fn bch2_extent_reconcile_validate(c: *mut c::bch_fs, k: c::bkey_s_c,
                                          from: *const c::bkey_validate_context,
                                          r: *const c::bch_extent_reconcile) -> c_int;
    pub fn bch2_extent_rebalance_v1_to_text(out: *mut c::printbuf, c: *mut c::bch_fs,
                                            r: *const c::bch_extent_rebalance_v1);
    pub fn bch2_extent_reconcile_to_text(out: *mut c::printbuf, c: *mut c::bch_fs,
                                         r: *const c::bch_extent_reconcile);
}
