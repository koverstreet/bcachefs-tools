// SPDX-License-Identifier: GPL-2.0

//! The data types of data/read_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_opaque, c_default};
use cstruct_macros::{bitfield, c_verbatim, CStruct};
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

        pub offset_into_extent: core::ffi::c_uint,

        pub flags: u16,
        #[c_anon]
        pub state: pub union bch_read_bio_state {
            #[c_anon]
            pub bits: pub struct bch_read_bio_state_bits {
                #[c_anon("")]
                pub __bitfield_align: [u16; 0],
                #[c_bitfield]
                #>[derive(Clone, Copy, CStruct, TypeInfo)]-
                #>[repr(C)]-
                #>[bitfield(u16, repr = zerocopy::byteorder::native_endian::U16, from = zerocopy::byteorder::native_endian::U16::new, into = zerocopy::byteorder::native_endian::U16::get)]
                pub data_update_bits: pub struct bch_read_bio_data_update_bits {
                    #[bits(1)]
                    pub data_update: u16,
                    #[bits(1)]
                    pub data_update_verify_decompress: u16,
                    #[bits(1)]
                    pub promote: u16,
                    #[bits(1)]
                    pub bounce: u16,
                    #[bits(1)]
                    pub split: u16,
                    #[bits(1)]
                    pub narrow_crcs: u16,
                    #[bits(1)]
                    pub saw_error: u16,
                    #[bits(1)]
                    pub self_healing: u16,
                    #[bits(2)]
                    pub context: u16,
                    #[bits(6)]
                    pub __pad: u8,
                },
            },
            pub _state: u16,
        },
        pub ret: i16,
        /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS: see types.rs */
        #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
        pub list_idx: core::ffi::c_uint,

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
impl bch_read_bio_state_bits {
    pub fn data_update(&self) -> u16 { let b = self.data_update_bits; b.data_update() }
    pub fn set_data_update(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_data_update(v); self.data_update_bits = b; }
    pub fn data_update_verify_decompress(&self) -> u16 { let b = self.data_update_bits; b.data_update_verify_decompress() }
    pub fn set_data_update_verify_decompress(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_data_update_verify_decompress(v); self.data_update_bits = b; }
    pub fn promote(&self) -> u16 { let b = self.data_update_bits; b.promote() }
    pub fn set_promote(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_promote(v); self.data_update_bits = b; }
    pub fn bounce(&self) -> u16 { let b = self.data_update_bits; b.bounce() }
    pub fn set_bounce(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_bounce(v); self.data_update_bits = b; }
    pub fn split(&self) -> u16 { let b = self.data_update_bits; b.split() }
    pub fn set_split(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_split(v); self.data_update_bits = b; }
    pub fn narrow_crcs(&self) -> u16 { let b = self.data_update_bits; b.narrow_crcs() }
    pub fn set_narrow_crcs(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_narrow_crcs(v); self.data_update_bits = b; }
    pub fn saw_error(&self) -> u16 { let b = self.data_update_bits; b.saw_error() }
    pub fn set_saw_error(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_saw_error(v); self.data_update_bits = b; }
    pub fn self_healing(&self) -> u16 { let b = self.data_update_bits; b.self_healing() }
    pub fn set_self_healing(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_self_healing(v); self.data_update_bits = b; }
    pub fn context(&self) -> u16 { let b = self.data_update_bits; b.context() }
    pub fn set_context(&mut self, v: u16) { let mut b = self.data_update_bits; b.set_context(v); self.data_update_bits = b; }
}

c_verbatim!(r#"
struct bch_devs_mask;
struct cache_promote_op;
"#);

c_opaque!(cache_promote_op);

c_verbatim!(r#"
struct extent_ptr_decoded;
struct promote_op;
"#);
