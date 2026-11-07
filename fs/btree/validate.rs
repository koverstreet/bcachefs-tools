// SPDX-License-Identifier: GPL-2.0

use crate::bkey_fsck_err_on;
use crate::btree::bkey::BkeySC;
use crate::btree::node::BtreeNode;
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::util::ffi::Opaque;
use core::ffi::c_int;
use cstruct_macros::c_export;

/// @v's key inside @b's range.
pub fn bkey_in_btree_node(v: &BkeyValidate<'_, '_>, b: &BtreeNode) -> Result<(), BchError> {
    let data = unsafe { b.contents() }.data();

    bkey_fsck_err_on!(v, v.k.k.p < data.min_key, id::bkey_before_start_of_btree_node,
                      "key before start of btree node")?;
    bkey_fsck_err_on!(v, v.k.k.p > data.max_key, id::bkey_after_end_of_btree_node,
                      "key past end of btree node")
}

#[c_export]
pub fn bch2_bkey_in_btree_node(c: &Opaque<Fs>, b: &BtreeNode, k: BkeySC<'_>,
                               from: &c::bkey_validate_context) -> c_int {
    ret_to_c(bkey_in_btree_node(&BkeyValidate { fs: Fs::from_c(c), k, from }, b))
}
