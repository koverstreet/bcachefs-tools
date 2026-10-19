// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_ulong, c_void};
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

c_extern! {
    pub fn bch2_btree_trans_to_text(out: *mut c::printbuf, trans: *mut c::btree_trans);
    pub fn bch2_path_put(trans: *mut c::btree_trans, path_idx: c::btree_path_idx_t, intent: bool);
    pub fn __bch2_trans_relock(trans: *mut c::btree_trans, trace: bool) -> c_int;
    pub fn bch2_trans_relock_notrace(trans: *mut c::btree_trans) -> c_int;
    pub fn __bch2_trans_kmalloc(trans: *mut c::btree_trans, size: usize,
                                ip: c_ulong) -> *mut c_void;
    pub fn bch2_trans_unlock(trans: *mut c::btree_trans);
    pub fn bch2_trans_unlock_long(trans: *mut c::btree_trans);
    pub fn bch2_trans_restart_error(trans: *mut c::btree_trans, restart_count: u32) -> !;
    pub fn bch2_trans_restart_ip(trans: *mut c::btree_trans, err: c_int, ip: c_ulong) -> c_int;
    #[must_use]
    pub fn bch2_btree_iter_traverse(iter: *mut c::btree_iter) -> c_int;
    pub fn bch2_btree_iter_peek_node(iter: *mut c::btree_iter) -> *mut c::btree;
    pub fn bch2_btree_iter_peek_max(iter: *mut c::btree_iter, end: *const c::bpos) -> c::bkey_s_c;
    pub fn bch2_btree_iter_peek_prev_min(iter: *mut c::btree_iter, end: c::bpos) -> c::bkey_s_c;
    pub fn bch2_btree_iter_peek_slot(iter: *mut c::btree_iter) -> c::bkey_s_c;
    pub fn bch2_btree_iter_prev_slot(iter: *mut c::btree_iter) -> c::bkey_s_c;
    pub fn bch2_btree_iter_advance(iter: *mut c::btree_iter) -> bool;
    pub fn bch2_btree_iter_rewind(iter: *mut c::btree_iter) -> bool;
    pub fn bch2_trans_iter_exit(iter: *mut c::btree_iter);
    pub fn bch2_trans_iter_init_outlined(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                         btree_id: c::btree_id, pos: c::bpos,
                                         flags: c::btree_iter_update_trigger_flags, ip: c_ulong);
    pub fn bch2_trans_copy_iter(dst: *mut c::btree_iter, src: *mut c::btree_iter);
    pub fn __bch2_trans_node_iter_init(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                       btree_id: c::btree_id, pos: c::bpos, locks_want: c_uint,
                                       depth: c_uint, flags: c::btree_iter_update_trigger_flags);
    pub fn bch2_trans_begin(trans: *mut c::btree_trans) -> u32;
    pub fn __bch2_trans_get(c: *mut c::bch_fs, fn_idx: c_uint) -> *mut c::btree_trans;
    pub fn bch2_trans_put(trans: *mut c::btree_trans);
    pub fn bch2_current_has_btree_trans(c: *mut c::bch_fs) -> bool;
    pub static mut bch2_btree_transaction_fns: [*const c_char; c::BCH_TRANSACTIONS_NR as usize];
    pub fn bch2_trans_get_fn_idx(fn_: *const c_char) -> c_uint;
}
