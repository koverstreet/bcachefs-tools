// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{bitfield, CStruct};
use nestify::nest;
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_read_err_report {
    pub lock: c::mutex,
    pub errors: u32,
    pub msg: c::printbuf,
}
c_default!(bch_read_err_report);

nest! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[repr(C)]*
    pub struct bch_read_bio {
        pub c: *mut c::bch_fs,
        pub ca: *mut c::bch_dev, /* stashed at submit; see bch_write_bio */
        pub start_time: u64,
        pub submit_time: u64,

        /*
         * Reads will often have to be split, and if the extent being read from
         * was checksummed or compressed we'll also have to allocate bounce
         * buffers and copy the data back into the original bio.
         *
         * If we didn't have to split, we have to save and restore the original
         * bi_end_io - @split below indicates which:
         */
        #[c_anon]
        pub completion: pub union bch_read_bio_completion {
            pub parent: *mut c::bch_read_bio,
            pub end_io: *mut c::bio_end_io_t,
        },

        /*
         * Saved copy of bio->bi_iter, from submission time - allows us to
         * resubmit on IO error, and also to copy data back to the original bio
         * when we're bouncing:
         */
        pub bvec_iter: c::bvec_iter,

        pub offset_into_extent: c_uint,

        pub flags: u16,
        #[c_anon]
        pub state: pub union bch_read_bio_state {
            #[c_anon]
            pub fields: pub struct bch_read_bio_state_fields {
                #[c_bitfield]
                #>[derive(Clone, Copy, CStruct, TypeInfo)]-
                #>[repr(C)]-
                #>[bitfield(u16)]
                pub bits: pub struct bch_read_bio_bits {
                    data_update:                   u1,
                    data_update_verify_decompress: u1,
                    promote:                       u1,
                    bounce:                        u1,
                    split:                         u1,
                    narrow_crcs:                   u1,
                    saw_error:                     u1,
                    self_healing:                  u1,
                    context:                       u2,
                    __pad:                         u6,
                },
            },
            pub _state: u16,
        },
        pub ret: i16,
        /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS, which bcachefs.h defines from these */
        #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
        pub list_idx: c_uint,

        pub pick: c::extent_ptr_decoded,

        /*
         * pos we read from - different from data_pos for indirect extents:
         */
        pub subvol: u32,
        pub read_pos: c::bpos,

        /*
         * start pos of data we read (may not be pos of data we want) - for
         * promote, narrow extents paths:
         */
        pub data_btree: c::btree_id,
        pub data_pos: c::bpos,
        pub version: c::bversion,

        pub opts: c::bch_inode_opts,

        pub failed: *mut c::bch_io_failures,
        pub err_report: *mut c::bch_read_err_report,

        pub work: c::work_struct,

        pub bio: c::bio,
    }
}
c_default!(bch_read_bio);
