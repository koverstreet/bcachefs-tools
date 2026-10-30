// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::Lock;
use crate::new_lock;
use crate::errcode::BchError;
use crate::types::{RustOnly, rust_only};
use crate::util::alloc::KVVec;
use cstruct_macros::{CStruct, Init, TypeInfo};
use pin_init::pin_data;

/*
 * A path that fsck damaged or couldn't fully recover, remembered so we can
 * print a short per-path summary at the end instead of burying it in the error
 * firehose. Keyed by (inum, snapshot); reasons is a bitmask of enum
 * bch_fsck_damage_type - deduped and OR'd on insert, so one line per path lists
 * everything that happened to it, and re-recording on transaction restart is a
 * no-op.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct fsck_damaged_path {
    pub inum: u64,
    pub snapshot: u32,
    /* bch_sb_error_id, deduped; 0 terminated (error 0 is fs-level) */
    pub nr_errors: u16,
    pub errors: [u16; 7],
}

/// One kind of fsck error's state this mount: how often it's been seen, the
/// last message - a restart reports it again, and replays the verdict - and
/// a Y or N for all of its kind: init/error.rs.
pub(crate) struct FsckErrState {
    pub(crate) id:          c::bch_sb_error_id,
    pub(crate) nr:          u64,
    /// Past FSCK_ERR_RATELIMIT_NR, the rest unprinted
    pub(crate) ratelimited: bool,
    /// The verdict, once there's been one
    pub(crate) ret:         Option<BchError>,
    /// FSCK_FIX_yes or FSCK_FIX_no for all of them: a Y or N answer
    pub(crate) fix:         Option<c::fsck_err_opts>,
    pub(crate) last_msg:    Option<KVVec<u8>>,
}

impl FsckErrState {
    pub(crate) fn new(id: c::bch_sb_error_id) -> Self {
        FsckErrState { id, nr: 0, ratelimited: false, ret: None, fix: None, last_msg: None }
    }
}

/// The error paths' state, under the errors lock: each fsck error's, and the
/// paths fsck damaged, for its summary.
pub(crate) struct ErrorsLocked {
    pub(crate) msgs:                    KVVec<FsckErrState>,
    pub(crate) msgs_alloc_err:          bool,
    pub(crate) damaged_paths:           KVVec<c::fsck_damaged_path>,
    pub(crate) damaged_paths_alloc_err: bool,
}

impl ErrorsLocked {
    fn new() -> Self {
        ErrorsLocked {
            msgs:                    KVVec::new(),
            msgs_alloc_err:          false,
            damaged_paths:           KVVec::new(),
            damaged_paths_alloc_err: false,
        }
    }
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_errors {
    /* What the error paths keep: init/error.rs */
    #[pin]
    #[init(RustOnly::new(new_lock!(ErrorsLocked::new())))]
    pub(crate) state: rust_only!(Lock<c::mutex, ErrorsLocked>),

    /* The superblock's error counts, by id: sb/errors.rs */
    #[pin]
    #[init(RustOnly::new(new_lock!(KVVec::new())))]
    pub(crate) counts: rust_only!(Lock<c::mutex, KVVec<c::bch_sb_error_entry_cpu>>),
}
