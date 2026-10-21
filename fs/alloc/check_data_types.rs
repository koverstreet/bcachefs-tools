// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of alloc/check_data.h - C gets it as prototypes, in
//! alloc/check_data_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

use crate::cstructs::c;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_bkey_drop_device_and_update(trans: *mut c::btree_trans, btree: c::btree_id, level: c_uint,
                                            k: c::bkey_s_c, dev: c_uint, err: c::bch_key_type_errors) -> c_int;
    pub fn bch2_kill_replica_if_checksum_bad(trans: *mut c::btree_trans, btree: c::btree_id, level: c_uint,
                                             k: c::bkey_s_c, other_btree: c::btree_id, other_k: c::bkey_s_c,
                                             dev: c_uint) -> c_int;
}
