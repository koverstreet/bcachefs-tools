// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/iter_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_extern};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct trans_for_each_path_inorder_iter {
    pub sorted_idx: c::btree_path_idx_t,
    pub path_idx: c::btree_path_idx_t,
}

/*
 * Exempt bch2_trans_begin() from the dropped-updates warning for a scope (see
 * begin_may_drop_updates), restoring the previous value on exit, so exemptions
 * nest: an inner one ending doesn't end the outer.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct trans_may_drop_updates {
    pub trans: *mut c::btree_trans,
    pub old: bool,
}
c_default!(trans_may_drop_updates);

// What Rust calls of btree/iter.h: C gets these as prototypes, in btree/iter_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_path_put(arg1: *mut c::btree_trans, arg2: c::btree_path_idx_t, arg3: bool);
    pub fn __bch2_trans_relock(arg1: *mut c::btree_trans, arg2: bool) -> core::ffi::c_int;
    pub fn bch2_trans_relock_notrace(arg1: *mut c::btree_trans) -> core::ffi::c_int;
    pub fn __bch2_trans_kmalloc(arg1: *mut c::btree_trans, arg2: usize, arg3: core::ffi::c_ulong) -> *mut core::ffi::c_void;
    pub fn bch2_trans_unlock(arg1: *mut c::btree_trans);
    pub fn bch2_trans_unlock_long(arg1: *mut c::btree_trans);
    pub fn bch2_trans_restart_error(arg1: *mut c::btree_trans, arg2: u32) -> !;
    pub fn bch2_trans_restart_ip(arg1: *mut c::btree_trans, arg2: core::ffi::c_int, arg3: core::ffi::c_ulong) -> core::ffi::c_int;
    pub fn bch2_btree_iter_traverse(arg1: *mut c::btree_iter) -> core::ffi::c_int;
    pub fn bch2_btree_iter_peek_node(arg1: *mut c::btree_iter) -> *mut c::btree;
    pub fn bch2_btree_iter_peek_max(arg1: *mut c::btree_iter, arg2: *const c::bpos) -> c::bkey_s_c;
    pub fn bch2_btree_iter_peek_prev_min(arg1: *mut c::btree_iter, arg2: c::bpos) -> c::bkey_s_c;
    pub fn bch2_btree_iter_peek_slot(arg1: *mut c::btree_iter) -> c::bkey_s_c;
    pub fn bch2_btree_iter_advance(arg1: *mut c::btree_iter) -> bool;
    pub fn bch2_btree_iter_rewind(arg1: *mut c::btree_iter) -> bool;
    pub fn bch2_trans_iter_exit(arg1: *mut c::btree_iter);
    pub fn bch2_trans_iter_init_outlined(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::btree_id, arg4: c::bpos, arg5: c::btree_iter_update_trigger_flags, ip: core::ffi::c_ulong);
    pub fn bch2_trans_copy_iter(arg1: *mut c::btree_iter, arg2: *mut c::btree_iter);
    pub fn __bch2_trans_node_iter_init(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::btree_id, arg4: c::bpos, arg5: core::ffi::c_uint, arg6: core::ffi::c_uint, arg7: c::btree_iter_update_trigger_flags);
    pub fn bch2_trans_begin(arg1: *mut c::btree_trans) -> u32;
    pub fn __bch2_trans_get(arg1: *mut c::bch_fs, arg2: core::ffi::c_uint) -> *mut c::btree_trans;
    pub fn bch2_trans_put(arg1: *mut c::btree_trans);
    pub fn bch2_current_has_btree_trans(arg1: *mut c::bch_fs) -> bool;
    #[c("const char *bch2_btree_transaction_fns[BCH_TRANSACTIONS_NR]")]
    pub static mut bch2_btree_transaction_fns: [*const crate::util::ffi::c_char; 128usize];
    pub fn bch2_trans_get_fn_idx(arg1: *const crate::util::ffi::c_char) -> core::ffi::c_uint;
}
