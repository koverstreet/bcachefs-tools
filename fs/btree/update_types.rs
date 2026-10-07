// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/update_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{c_enum, c_extern, c_verbatim, c_xmacro};

c_verbatim!(r#"
struct bch_fs;

struct btree;
"#);

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

macro_rules! ____bch_trans_commit_flags_0 {
    ([$($acc:tt)*] $(($n:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum __bch_trans_commit_flags: u32 {
                $($acc)*
                $([<__BCH_TRANS_COMMIT_ $n>],)*
            }
        }
    } };
}
BCH_TRANS_COMMIT_FLAGS!(____bch_trans_commit_flags_0 [
    /* First bits for bch_watermark: */
    __BCH_TRANS_COMMIT_FLAGS_START = c::BCH_WATERMARK_BITS,
]);

macro_rules! __bch_trans_commit_flags_0 {
    ([$($acc:tt)*] $(($n:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[flags]
            pub enum bch_trans_commit_flags: u32 {
                $($acc)*
                $([<BCH_TRANS_COMMIT_ $n>] = 1 << (c::[<__BCH_TRANS_COMMIT_ $n>] as u32),)*
            }
        }
    } };
}
BCH_TRANS_COMMIT_FLAGS!(__bch_trans_commit_flags_0 []);

// What Rust calls of btree/update.h: C gets these as prototypes, in btree/update_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_btree_delete_at(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::btree_iter_update_trigger_flags) -> core::ffi::c_int;
    pub fn bch2_btree_delete(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: c::btree_iter_update_trigger_flags) -> core::ffi::c_int;
    pub fn bch2_btree_insert_nonextent(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: *mut c::bkey_i, arg4: core::ffi::c_uint, arg5: c::btree_iter_update_trigger_flags) -> core::ffi::c_int;
    pub fn bch2_btree_insert_trans(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: *mut c::bkey_i, arg4: c::btree_iter_update_trigger_flags) -> core::ffi::c_int;
    pub fn bch2_btree_insert(arg1: *mut c::bch_fs, arg2: c::btree_id, arg3: *mut c::bkey_i, arg4: *mut c::disk_reservation, arg5: c::bch_trans_commit_flags, arg6: c::btree_iter_update_trigger_flags) -> core::ffi::c_int;
    pub fn bch2_btree_delete_range_trans(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: c::bpos, arg5: c::btree_iter_update_trigger_flags) -> core::ffi::c_int;
    pub fn bch2_btree_delete_range(arg1: *mut c::bch_fs, arg2: c::btree_id, arg3: c::bpos, arg4: c::bpos, arg5: c::btree_iter_update_trigger_flags) -> core::ffi::c_int;
    pub fn bch2_btree_bit_mod(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: bool) -> core::ffi::c_int;
    pub fn bch2_btree_bit_mod_buffered(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: bool) -> core::ffi::c_int;
    pub fn __bch2_insert_snapshot_whiteouts(arg1: *mut c::btree_trans, arg2: c::btree_id, arg3: c::bpos, arg4: *mut c::snapshot_id_list) -> core::ffi::c_int;
    pub fn bch2_trans_update_ip(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: *mut c::bkey_i, arg4: core::ffi::c_uint, arg5: c::btree_iter_update_trigger_flags, arg6: core::ffi::c_ulong) -> core::ffi::c_int;
    pub fn bch2_trans_update_extent_overwrite(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::btree_iter_update_trigger_flags, arg4: c::bkey_s_c, arg5: c::bkey_s_c) -> core::ffi::c_int;
    pub fn bch2_bkey_get_empty_slot(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::btree_id, arg4: c::bpos, arg5: c::bpos) -> core::ffi::c_int;
    pub fn bch2_trans_commit_hook(arg1: *mut c::btree_trans, arg2: *mut c::btree_trans_commit_hook);
    pub fn __bch2_trans_commit(arg1: *mut c::btree_trans, arg2: c::bch_trans_commit_flags, arg3: bool) -> core::ffi::c_int;
}
