// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::projections::Volatile;
use core::ffi::c_uint;
use core::sync::atomic::{AtomicI32, AtomicI64};
use cstruct_macros::{c_name, CStruct, TypeInfo};
use nestify::nest;
use crate::format::{Bpos, DataType};

nest! {
    /// What moves count into - whoever issues them, and their completions,
    /// on any thread - and what a data job's progress reads as it goes.
    /// Shared throughout, so every field is a Volatile, written one writer at
    /// a time, or an atomic, by many.
    #[c_name(bch_move_stats)]
    #[derive(CStruct, TypeInfo)]*
    #[repr(C)]*
    pub struct MoveStats {
        pub ret: Volatile<c::bch_ioctl_data_event_ret>,

        /* Where the walk is: data_type and pos, or for phys, dev and offset */
        pub at: pub struct bch_move_stats_at {
            pub data_type: Volatile<DataType>,
            pub pos: Volatile<c::bbpos>,
            pub dev: Volatile<c_uint>,
            pub offset: Volatile<u64>,
        },

        pub keys_moved: AtomicI64,
        pub keys_raced: AtomicI64,
        pub sectors_seen: AtomicI64,
        pub sectors_moved: AtomicI64,
        pub sectors_raced: AtomicI64,
        pub sectors_error_corrected: AtomicI64,
        pub sectors_error_uncorrected: AtomicI64,
    }
}
c_default!(MoveStats);

/// C's name, as FFI declarations and the structs pointing at one spell it.
pub type bch_move_stats = MoveStats;

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, CStruct)]
pub struct move_bucket_key {
    pub bucket: Bpos,
    pub generation: c_uint,
}

/*
 * A bucket copygc's evacuating, as the moves see it: each holds a count until
 * its update is done. The rest of what copygc keeps of it is Rust's,
 * data/copygc.rs.
 */
#[repr(C)]
#[derive(CStruct)]
pub struct move_bucket {
    pub count: AtomicI32,
}
