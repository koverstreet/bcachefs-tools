// SPDX-License-Identifier: GPL-2.0

//! The data types of data/move_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_typedef, c_verbatim, CStruct};

c_verbatim!(r#"
struct bch_inode_opts;
struct bch_read_bio;
struct btree_trans;
struct data_update_opts;
"#);

/*
 * Tracks in-flight data movement IO for ratelimiting.
 *
 * Four atomic counters track sectors and IOs in flight:
 *  - read_sectors/read_ios: extent read submit -> read completion
 *  - write_sectors/write_ios: read completion -> write completion
 *
 * bch2_move_ratelimit() blocks the caller until all counters are below
 * c->opts.move_bytes_in_flight / move_ios_in_flight.
 *
 * Extent moves (bch2_move_extent) and stripe repairs (bch2_stripe_repair)
 * both account through these counters.
 *
 * Lifetime: every in-flight IO holds closure_get(&ctxt->cl).
 * bch2_moving_ctxt_flush_all() waits for all IO via closure_sync(),
 * and bch2_moving_ctxt_exit() asserts all counters are zero.
 */
#[repr(C)]
#[derive(CStruct)]
pub struct moving_context {
    pub trans: *mut c::btree_trans,
    pub list: c::list_head,
    pub fn_: *mut core::ffi::c_void,

    pub rate: *mut c::bch_ratelimit,
    pub stats: *mut c::bch_move_stats,
    pub wp: c::write_point_specifier,
    pub wait_on_copygc: bool,

    /* For waiting on outstanding reads and writes: */
    pub cl: c::closure,

    pub lock: c::mutex,
    pub reads: c::list_head,
    pub ios: c::list_head,
    pub io_seq: u64,

    /* in flight sectors: */
    pub read_sectors: c::atomic_t,
    pub write_sectors: c::atomic_t,
    pub read_ios: c::atomic_t,
    pub write_ios: c::atomic_t,

    pub wait: c::wait_queue_head_t,
}
c_default!(moving_context);

c_typedef! {
    pub type move_pred_fn = Option<unsafe extern "C" fn(*mut c::btree_trans, *mut core::ffi::c_void, c::btree_id, c::bkey_s_c,
                                                        *mut c::bch_inode_opts, *mut c::data_update_opts) -> core::ffi::c_int>;
}
