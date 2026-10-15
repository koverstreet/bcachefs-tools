// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::btree::bkey::BkeyI;
use crate::cstructs::c;
use crate::types::{bits_to_longs, c_default};
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use cstruct_macros::{c_enum, CStruct};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct ec_bio {
    pub ca: *mut c::bch_dev,
    pub buf: *mut c::ec_stripe_buf,
    pub idx: usize,
    pub rw: c_int,
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
            pub offset: c_uint,
            pub size: c_uint,
            pub err: [[i16; c::BCH_BKEY_PTRS_MAX as usize]; 2],
            pub data: [*mut c_void; c::BCH_BKEY_PTRS_MAX as usize],

            /* Stale when we read the stripe key, i.e. alloc inconsistency */
            #[c("unsigned long stale[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)]")]
            pub stale: [c_ulong; bits_to_longs(c::BCH_BKEY_PTRS_MAX as usize)],

            pub csum_good: [c::bch_csum; c::BCH_BKEY_PTRS_MAX as usize],
            pub csum_bad: [c::bch_csum; c::BCH_BKEY_PTRS_MAX as usize],

            pub key: BkeyI<c::Stripe>,
            pub pad: [u64; 255],
        },
    }
}
c_default!(ec_stripe_buf);
