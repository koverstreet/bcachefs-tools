// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint, c_ulong};
use cstruct_macros::{c_enum, c_extern, c_xmacro};

c_xmacro! {
    BCH_TRANS_COMMIT_FLAGS(x) {
        (no_enospc, "don't check for enospc"),
        (no_check_rw, "don't attempt to take a ref on c->writes"),
        (no_journal_res, "don't take a journal reservation, instead " "pin journal entry referred to by trans->journal_res.seq"),
        (no_skip_noops, "don't drop noop updates"),
        (journal_reclaim, "operation required for journal reclaim; may return error" "instead of deadlocking if BCH_WATERMARK_reclaim not specified"),
        (journal_replay, "in journal replay"),
        (skip_accounting_apply, "we're in journal replay - accounting updates have already been applied"),
    }
}

c_enum! {
    #[closed]
    pub enum __bch_trans_commit_flags: u32 {
        /* First bits for bch_watermark: */
        __BCH_TRANS_COMMIT_FLAGS_START = c::BCH_WATERMARK_BITS,
        BCH_TRANS_COMMIT_FLAGS(n, ..) => __BCH_TRANS_COMMIT_ ## n,
    }
}

c_enum! {
    #[flags]
    pub enum bch_trans_commit_flags: u32 {
        BCH_TRANS_COMMIT_FLAGS(n, ..) => BCH_TRANS_COMMIT_ ## n = 1 << (c::__BCH_TRANS_COMMIT_ ## n as u32),
    }
}

c_extern! {
    pub fn bch2_btree_delete_at(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert_nonextent(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: *mut c::bkey_i, arg4: c_uint, arg5: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert_trans(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: *mut c::bkey_i, arg4: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert(arg1: *mut c::bch_fs, arg2: c::btree_id, arg3: *mut c::bkey_i, arg4: *mut c::disk_reservation, arg5: c::bch_trans_commit_flags, arg6: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete_range_trans(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: c::bpos, arg5: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete_range(arg1: *mut c::bch_fs, arg2: c::btree_id, arg3: c::bpos, arg4: c::bpos, arg5: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_bit_mod(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: bool) -> c_int;
    pub fn bch2_btree_bit_mod_buffered(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: bool) -> c_int;
    pub fn __bch2_insert_snapshot_whiteouts(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: *mut c::snapshot_id_list) -> c_int;
    pub fn bch2_trans_update_ip(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: *mut c::bkey_i, arg4: c_uint, arg5: c::btree_iter_update_trigger_flags, arg6: c_ulong) -> c_int;
    pub fn bch2_trans_update_extent_overwrite(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::btree_iter_update_trigger_flags, arg4: c::bkey_s_c, arg5: c::bkey_s_c) -> c_int;
    pub fn bch2_bkey_get_empty_slot(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::btree_id, arg4: c::bpos, arg5: c::bpos) -> c_int;
    pub fn bch2_trans_commit_hook(arg1: *mut c::btree_trans, arg2: *mut c::btree_trans_commit_hook);
    pub fn __bch2_trans_commit(arg1: *mut c::btree_trans, arg2: c::bch_trans_commit_flags, arg3: bool) -> c_int;
}
