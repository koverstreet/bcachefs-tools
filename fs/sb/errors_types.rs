// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use crate::util::ffi::c_char;
use cstruct_macros::{CStruct, bitfield, c_extern, c_typedef};
use nestify::nest;
use typeinfo_macros::TypeInfo;

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
    pub struct bch_sb_error_entry_cpu {
        #[c_bitfield]
        #>[bitfield(u64)]
        pub bits: pub struct bch_sb_error_entry_cpu_bits {
            #[bits(16)]
            pub id: u64,
            #[bits(48)]
            pub nr: u64,
        },
        pub first_error_time: u64,
        pub last_error_time: u64,
    }
}

c_typedef! {
    #[c("DARRAY(struct bch_sb_error_entry_cpu) bch_sb_errors_cpu")]
    pub type bch_sb_errors_cpu = DArray<c::bch_sb_error_entry_cpu>;
}

c_extern! {
    pub static bch2_sb_error_strs: [*const c_char; 0];
    pub fn bch2_sb_error_count(c: *mut c::bch_fs, err: c::bch_sb_error_id);
}
