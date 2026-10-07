// SPDX-License-Identifier: GPL-2.0

//! The data types of data/ec/io_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{bits_to_longs, c_default};
use cstruct_macros::{c_enum, c_verbatim, CStruct};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct ec_bio {
    pub ca: *mut c::bch_dev,
    pub buf: *mut c::ec_stripe_buf,
    pub idx: usize,
    pub rw: core::ffi::c_int,
    pub submit_time: u64,
    pub bio: c::bio,
}
c_default!(ec_bio);

c_enum! {
    #[closed]
    pub enum bch_stripe_buf_err: u32 {
        STRIPE_BUF_PRE_RECOV,
        STRIPE_BUF_POST_RECOV,
    }
}

nest! {
    #[derive(CStruct)]*
    #[repr(C)]*
    pub struct ec_stripe_buf {
        /* belongs to the buffer's owner, see bch2_ec_stripe_buf_move(): */
        pub io: c::closure,

        #[c_struct_group]
        pub contents: pub struct ec_stripe_buf_contents {
            pub c: *mut c::bch_fs,

            /* might not be buffering the entire stripe: */
            pub offset: core::ffi::c_uint,
            pub size: core::ffi::c_uint,
            pub err: [[i16; c::BCH_BKEY_PTRS_MAX as usize]; 2],
            pub data: [*mut core::ffi::c_void; c::BCH_BKEY_PTRS_MAX as usize],

            /* Stale when we read the stripe key, i.e. alloc inconsistency */
            #[c("unsigned long stale[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)]")]
            pub stale: [core::ffi::c_ulong; bits_to_longs(c::BCH_BKEY_PTRS_MAX as usize)],

            pub csum_good: [c::bch_csum; c::BCH_BKEY_PTRS_MAX as usize],
            pub csum_bad: [c::bch_csum; c::BCH_BKEY_PTRS_MAX as usize],

            pub key: c::bkey_i_stripe,
            pub pad: [u64; 255],
        },
    }
}
c_default!(ec_stripe_buf);

c_verbatim!(r#"
struct bch_read_bio;
"#);
