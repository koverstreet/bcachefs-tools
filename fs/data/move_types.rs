// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::util::Printbuf;
use crate::types::{DArray, c_default};
use crate::util::projections::Volatile;
use core::ffi::{c_int, c_uint, c_void};
use core::sync::atomic::{AtomicI32, AtomicI64};
use cstruct_macros::{CStruct, c_extern, c_typedef, TypeInfo};
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
        pub phys: Volatile<bool>,
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

c_extern! {
    pub fn bch2_moving_ctxt_exit(ctxt: *mut c::moving_context);
    pub fn bch2_moving_ctxt_init(ctxt: *mut c::moving_context, c: *mut c::bch_fs,
                                 rate: *mut c::bch_ratelimit, stats: *mut c::bch_move_stats,
                                 wp: c::write_point_specifier, wait_on_copygc: bool);
    pub fn bch2_move_data_btree(ctxt: *mut c::moving_context, start: c::bpos, end: c::bpos,
                                pred: c::move_pred_fn, arg: *mut c_void, btree_id: c::btree_id,
                                level: c_uint) -> c_int;
    pub fn bch2_fs_moving_ctxts_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
    pub fn bch2_data_job(c: *mut c::bch_fs, stats: *mut c::bch_move_stats,
                         op: *mut c::bch_ioctl_data) -> c_int;
}

impl Fs {
    pub fn moving_ctxts_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_fs_moving_ctxts_to_text(out.as_raw(), self.as_ptr()) }
    }
}
