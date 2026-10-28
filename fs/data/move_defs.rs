// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::data::moving::InFlightUpdate;
use crate::types::Fifo;
use core::cell::{Cell, UnsafeCell};
use crate::util::task::RunQueue;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicI32, AtomicPtr};
use crate::util::ffi::Opaque;
use cstruct_macros::{c_typedef, CStruct};

/*
 * Tracks in-flight data movement IO for ratelimiting.
 *
 * Four atomic counters track sectors and IOs in flight:
 *  - read_sectors/read_ios: extent read submit -> its task taking the
 *    completed read
 *  - write_sectors/write_ios: then -> write completion
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
    /* on c->moving_context_list, which other contexts' adds and dels write */
    #[c("struct list_head list")]
    pub list: Opaque<c::list_head>,

    /* set racily, read by the listing */
    #[c("struct bch_move_stats *stats")]
    pub stats: AtomicPtr<c::bch_move_stats>,
    pub wp: c::write_point_specifier,
    pub wait_on_copygc: bool,

    /* For waiting on outstanding reads and writes: */
    #[c("struct closure cl")]
    pub cl: Opaque<c::closure>,

    pub io: moving_io_counts,

    /*
     * Its updates' tasks, run in its waits - data/moving_update.rs - whose
     * EventCount every completion wakes: bch2_moving_ctxt_wake()
     */
    pub runq: RunQueue,
    /*
     * Its data updates in flight, in the order they were issued, by seq - an
     * update's index in it - and the first whose write hasn't started. The
     * mover's thread's alone: data/moving.rs
     */
    pub updates: UnsafeCell<Fifo<InFlightUpdate, u64>>,
    pub write_next: Cell<u64>,
}
c_default!(moving_context);

/* In flight: what ratelimit() throttles on, and flush_all() waits out */
#[repr(C)]
#[derive(CStruct, Default)]
pub struct moving_io_counts {
    pub read_sectors: AtomicI32,
    pub write_sectors: AtomicI32,
    pub read_ios: AtomicI32,
    pub write_ios: AtomicI32,
}

c_typedef! {
    pub type move_pred_fn = Option<unsafe extern "C" fn(*mut c::btree_trans, *mut c_void, c::btree_id, c::bkey_s_c,
                                                        *mut c::bch_inode_opts, *mut c::data_update_opts) -> c_int>;
}
