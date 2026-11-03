// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::data::moving::InFlightUpdate;
use crate::types::Fifo;
use core::cell::{Cell, UnsafeCell};
use crate::util::task::RunQueue;
use core::sync::atomic::{AtomicI32, AtomicPtr};
use crate::util::gather::Gather;
use crate::util::task::waiter;
use cstruct_macros::CStruct;
use pin_init::pin_data;

/*
 * Tracks in-flight data movement IO for ratelimiting.
 *
 * Four atomic counters track sectors and IOs in flight:
 *  - read_sectors/read_ios: extent read submit -> its task taking the
 *    completed read
 *  - write_sectors/write_ios: then -> write completion
 *
 * Moving::ratelimit() blocks the caller until all counters are below
 * c->opts.move_bytes_in_flight / move_ios_in_flight.
 *
 * Moves (Moving::move_extent()) and stripe repairs (ec/repair.rs)
 * both account through these counters.
 *
 * Lifetime: every in-flight IO holds a ref on in_flight.
 * Moving::flush_all() waits for the counters to drain, and MovingCtxt's
 * drop asserts they have, then waits in_flight's refs out.
 */
#[pin_data]
#[repr(C)]
#[derive(CStruct)]
pub struct moving_context {
    pub trans: *mut c::btree_trans,

    #[c("struct bch_move_stats *stats")]
    pub stats: AtomicPtr<c::bch_move_stats>,
    pub wp: c::write_point_specifier,
    pub wait_on_copygc: bool,

    /* A ref each for what's in flight - MovingIo's - which drop waits out */
    #[pin]
    pub in_flight: Gather<(), waiter::Sync>,

    pub io: moving_io_counts,

    /*
     * Its updates' tasks, run in its waits - data/moving_update.rs - whose
     * EventCount every completion wakes: Moving::wake()
     */
    #[pin]
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
