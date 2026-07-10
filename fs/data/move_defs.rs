// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::{c_typedef, CStruct, c_enum, c_xmacro};

c_xmacro! {
    MOVE_LIMIT_SOURCES(x) {
        (default),
        (copygc_bucket),
        (reconcile_target),
        (phys_dev),
    }
}

c_enum! {
    #[closed]
    pub enum move_limit_source: u32 {
        MOVE_LIMIT_SOURCES(n) => MOVE_LIMITS_ ## n,
    }
}

c_xmacro! {
    MOVE_ROTATIONAL_LIMITS(x) {
        (background),
        (hipri),
    }
}

c_enum! {
    #[closed]
    pub enum move_rotational_limit: u32 {
        MOVE_ROTATIONAL_LIMITS(n) => MOVE_ROTATIONAL_LIMIT_ ## n,
    }
}
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
    pub fn_: *mut c_void,

    pub rate: *mut c::bch_ratelimit,
    pub stats: *mut c::bch_move_stats,
    pub wp: c::write_point_specifier,
    pub wait_on_copygc: bool,
    pub max_sectors_in_flight: c_uint,
    pub max_ios_in_flight: c_uint,
    pub limit_source: c::move_limit_source,
    pub limit_source_id: c_uint,

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
    pub type move_pred_fn = Option<unsafe extern "C" fn(*mut c::btree_trans, *mut c_void, c::btree_id, c::bkey_s_c,
                                                        *mut c::bch_inode_opts, *mut c::data_update_opts) -> c_int>;
}
