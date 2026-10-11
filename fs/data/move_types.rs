// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::{CStruct, c_extern, c_typedef};
use nestify::nest;
use typeinfo_macros::TypeInfo;

nest! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[repr(C)]*
    pub struct bch_move_stats {
        pub name: [c_char; 32],
        pub phys: bool,
        pub ret: c::bch_ioctl_data_event_ret,

        #[c_anon]
        pub at: pub union bch_move_stats_at {
            #[c_anon]
            pub logical: pub struct bch_move_stats_logical {
                pub data_type: c::bch_data_type,
                pub pos: c::bbpos,
            },
            #[c_anon]
            pub phys: pub struct bch_move_stats_phys {
                pub dev: c_uint,
                pub offset: u64,
            },
        },

        pub keys_moved: c::atomic64_t,
        pub keys_raced: c::atomic64_t,
        pub sectors_seen: c::atomic64_t,
        pub sectors_moved: c::atomic64_t,
        pub sectors_raced: c::atomic64_t,
        pub sectors_error_corrected: c::atomic64_t,
        pub sectors_error_uncorrected: c::atomic64_t,
        pub devs_error_uncorrected: c::bch_devs_mask,
    }
}
c_default!(bch_move_stats);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct move_bucket_key {
    pub bucket: c::bpos,
    pub generation: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct move_bucket {
    pub next: *mut c::move_bucket,
    pub hash: c::rhash_head,
    pub k: c::move_bucket_key,
    pub sectors: c_uint,
    pub count: c::atomic_t,
}
c_default!(move_bucket);

/*
 * What the journal scrub found wrong with an extent, for
 * bch2_scrub_journal_do_repairs() once we're rw: the scrub runs with journal
 * keys frozen, so it can neither repair nor record damage itself.
 *
 * @bad_devs is a mask of pointers by their position in @k, like ptrs_io_error,
 * not of device indices. @read_err is set when reading them failed outright,
 * rather than a replica being found bad on a read that succeeded.
 *
 * @level is 0 for an extent; for a btree node pointer it's the level @k lives
 * at, one above the node's.
 */
#[repr(C)]
#[derive(CStruct)]
#[c_typedef]
pub struct scrub_journal_repair {
    pub btree_id: c::btree_id,
    pub level: c_uint,
    pub bad_devs: c_uint,
    pub read_err: c_int,
    pub k: c::bkey_i,
    pub k_pad: [u64; c::BKEY_EXTENT_VAL_U64s_MAX],
}
c_default!(scrub_journal_repair);

c_typedef! {
    pub type darray_scrub_journal_repair = DArray<c::scrub_journal_repair>;
}

c_extern! {
    pub fn bch2_moving_ctxt_exit(ctxt: *mut c::moving_context);
    pub fn bch2_moving_ctxt_init(ctxt: *mut c::moving_context, c: *mut c::bch_fs, rate: *mut c::bch_ratelimit, stats: *mut c::bch_move_stats, wp: c::write_point_specifier, wait_on_copygc: bool);
    pub fn bch2_move_data_btree(ctxt: *mut c::moving_context, start: c::bpos, end: c::bpos, pred: c::move_pred_fn, arg: *mut c_void, btree_id: c::btree_id, level: c_uint) -> c_int;
}
