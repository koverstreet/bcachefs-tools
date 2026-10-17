// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::DArray;
use core::sync::atomic::AtomicU32;
use cstruct_macros::{CStruct, Init, TypeInfo};
use pin_init::pin_data;

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_logged_ops {
    /*
     * Test knob: KEY_TYPE_logged_op_* whose next cursor update should fail,
     * or 0. Armed via sysfs, one shot, disarms itself as it fires.
     */
    #[c("unsigned fail_next")]
    #[init(AtomicU32::new(0))]
    pub fail_next: AtomicU32,
    /*
     * Logged ops in the btree when we went rw - the ones from before this
     * mount, which recovery resumes. Positions in BTREE_ID_logged_ops,
     * ascending: see bch2_logged_ops_note_unfinished().
     */
    pub unfinished: DArray<u64>,
}
