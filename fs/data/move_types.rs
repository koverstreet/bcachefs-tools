// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use crate::util::projections::Volatile;
use core::ffi::{c_int, c_uint};
use core::sync::atomic::{AtomicI32, AtomicI64};
use cstruct_macros::{CStruct, c_typedef, TypeInfo};
use nestify::nest;

nest! {
    /// What moves count into - whoever issues them, and their completions,
    /// on any thread - and what the moving contexts' listing and a data job's
    /// progress read as it goes. Shared throughout, so every field is a
    /// Volatile, written one writer at a time, or an atomic, by many.
    #[derive(CStruct, TypeInfo)]*
    #[repr(C)]*
    #[c_name(bch_move_stats)]
    pub struct MoveStats {
        #[c("char name[32]")]
        pub name: [Volatile<u8>; 32],
        #[c("enum bch_ioctl_data_event_ret ret")]
        pub ret: Volatile<c::bch_ioctl_data_event_ret>,

        /* Where the walk is: data_type and pos, or for phys, dev and offset */
        #[c_anon]
        pub at: pub struct bch_move_stats_at {
            #[c("enum bch_data_type data_type")]
            pub data_type: Volatile<c::bch_data_type>,
            #[c("struct bbpos pos")]
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
        #[c("struct bch_devs_mask devs_error_uncorrected")]
        pub devs_error_uncorrected: c::AtomicDevsMask,
    }
}
c_default!(MoveStats);

/// C's name, as FFI declarations and the structs pointing at one spell it.
pub type bch_move_stats = MoveStats;

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, CStruct)]
pub struct move_bucket_key {
    pub bucket: c::bpos,
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
