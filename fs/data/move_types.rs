// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::projections::Volatile;
use core::ffi::c_uint;
use core::sync::atomic::AtomicI64;
use cstruct_macros::{MaybeZeroable, TypeInfo};
use nestify::nest;
use crate::format::{Bpos, DataType};

nest! {
    /// What moves count into - whoever issues them, and their completions,
    /// on any thread - and what a data job's progress reads as it goes.
    /// Shared throughout, so every field is a Volatile, written one writer at
    /// a time, or an atomic, by many.
    #[derive(Default, MaybeZeroable, TypeInfo)]*
    #[repr(C)]*
    pub struct MoveStats {
        pub ret: Volatile<c::IoctlDataEventRet>,

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

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, MaybeZeroable)]
pub struct move_bucket_key {
    pub bucket: Bpos,
    pub generation: c_uint,
}
