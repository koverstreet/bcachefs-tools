// SPDX-License-Identifier: GPL-2.0

//! The data types of data/move_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{CStruct, c_extern, c_verbatim};
use nestify::nest;
use typeinfo_macros::TypeInfo;


nest! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[repr(C)]*
    pub struct bch_move_stats {
        pub name: [crate::util::ffi::c_char; 32],
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
                pub dev: core::ffi::c_uint,
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
    pub generation: core::ffi::c_uint,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct move_bucket {
    pub next: *mut c::move_bucket,
    pub hash: c::rhash_head,
    pub k: c::move_bucket_key,
    pub sectors: core::ffi::c_uint,
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
    pub level: core::ffi::c_uint,
    pub bad_devs: core::ffi::c_uint,
    pub read_err: core::ffi::c_int,
    pub k: c::bkey_i,
    pub k_pad: [u64; c::BKEY_EXTENT_VAL_U64s_MAX],
}
c_default!(scrub_journal_repair);

c_verbatim!(r#"
DEFINE_DARRAY(scrub_journal_repair);
"#);

pub type darray_scrub_journal_repair = DArray<c::scrub_journal_repair>;

// What Rust calls of data/move.h: C gets these as prototypes, in data/move_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_moving_ctxt_exit(arg1: *mut c::moving_context);
    pub fn bch2_moving_ctxt_init(arg1: *mut c::moving_context, arg2: *mut c::bch_fs, arg3: *mut c::bch_ratelimit, arg4: *mut c::bch_move_stats, arg5: c::write_point_specifier, arg6: bool);
    pub fn bch2_move_data_btree(arg1: *mut c::moving_context, arg2: c::bpos, arg3: c::bpos, arg4: c::move_pred_fn, arg5: *mut core::ffi::c_void, arg6: c::btree_id, arg7: core::ffi::c_uint) -> core::ffi::c_int;
}
