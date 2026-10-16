// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint, c_ulong};
use cstruct_macros::{c_enum, c_extern, c_xmacro};

c_xmacro! {
    BCH_TRANS_COMMIT_FLAGS(x) {
        (no_enospc, "don't check for enospc"),
        (no_check_rw, "don't attempt to take a ref on c->writes"),
        (no_journal_res,
         "don't take a journal reservation, instead "
         "pin journal entry referred to by trans->journal_res.seq"),
        (no_skip_noops, "don't drop noop updates"),
        (journal_reclaim,
         "operation required for journal reclaim; may return error "
         "instead of deadlocking if BCH_WATERMARK_reclaim not specified"),
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
    pub fn bch2_trans_log_msg(trans: *mut c::btree_trans, buf: *mut c::printbuf) -> c_int;
    pub fn bch2_btree_delete_at(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete(trans: *mut c::btree_trans, btree: c::btree_id, pos: c::bpos,
                             flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert_nonextent(trans: *mut c::btree_trans, btree: c::btree_id,
                                       k: *mut c::bkey_i, k_buf_u64s: c_uint,
                                       flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert_trans(trans: *mut c::btree_trans, btree: c::btree_id,
                                   k: *mut c::bkey_i,
                                   flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert(c: *mut c::bch_fs, id: c::btree_id, k: *mut c::bkey_i,
                             disk_res: *mut c::disk_reservation,
                             commit_flags: c::bch_trans_commit_flags,
                             iter_flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete_range_trans(trans: *mut c::btree_trans, btree: c::btree_id,
                                         start: c::bpos, end: c::bpos,
                                         flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete_range(c: *mut c::bch_fs, id: c::btree_id, start: c::bpos, end: c::bpos,
                                   flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_bit_mod(trans: *mut c::btree_trans, btree: c::btree_id, pos: c::bpos,
                              set: bool) -> c_int;
    pub fn bch2_btree_bit_mod_buffered(trans: *mut c::btree_trans, btree: c::btree_id, pos: c::bpos,
                                       set: bool) -> c_int;
    pub fn __bch2_insert_snapshot_whiteouts(trans: *mut c::btree_trans, btree: c::btree_id,
                                            pos: c::bpos, s: *mut c::snapshot_id_list) -> c_int;
    #[must_use]
    pub fn bch2_trans_update_ip(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                k: *mut c::bkey_i, k_buf_u64s: c_uint,
                                flags: c::btree_iter_update_trigger_flags, ip: c_ulong) -> c_int;
    pub fn bch2_trans_update_extent_overwrite(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                              flags: c::btree_iter_update_trigger_flags,
                                              old: c::bkey_s_c, new: c::bkey_s_c) -> c_int;
    pub fn bch2_bkey_get_empty_slot(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                    btree: c::btree_id, start: c::bpos, end: c::bpos) -> c_int;
    pub fn bch2_trans_commit_hook(trans: *mut c::btree_trans, h: *mut c::btree_trans_commit_hook);
    pub fn __bch2_trans_commit(trans: *mut c::btree_trans, flags: c::bch_trans_commit_flags,
                               lazy: bool) -> c_int;
}
