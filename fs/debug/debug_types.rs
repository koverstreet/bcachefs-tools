// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::fs::Fs;
use crate::cstructs::c;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_btree_node_ondisk_to_text(out: *mut c::printbuf, c: *mut Fs,
                                          b: *const c::BtreeNode);
}
