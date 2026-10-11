// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::{CStruct, bitfield, c_extern, c_typedef};
use nestify::nest;

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, CStruct)]
    pub struct btree_read_bio {
        pub c: *mut c::bch_fs,
        pub ca: *mut c::bch_dev, /* stashed at submit; see bch_write_bio */
        pub b: *mut c::btree,
        pub start_time: u64,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct btree_read_bio_bits {
            #[bits(7)]
            pub idx: u32,
            #[bits(1)]
            pub __pad: u8,
        },
        /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS, which bcachefs.h defines from these */
        #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
        pub list_idx: c_uint,
        pub pick: c::extent_ptr_decoded,
        pub work: c::work_struct,
        pub bio: c::bio,
    }
}
c_default!(btree_read_bio);

c_typedef! {
    pub type btree_node_scrub_report_fn = Option<unsafe extern "C" fn(priv_: *mut c_void,
                                                                     dev: c_uint, good: bool)>;
}

c_extern! {
    pub fn bch2_btree_node_read_done(c: *mut c::bch_fs, ca: *mut c::bch_dev, b: *mut c::btree,
                                     failed: *mut c::bch_io_failures,
                                     err_msg: *mut c::printbuf) -> c_int;
}
