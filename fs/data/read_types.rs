// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::Printbuf;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{bitfield, c_extern, CStruct, TypeInfo};
use nestify::nest;

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_read_err_report {
    pub lock: c::mutex,
    pub errors: u32,
    pub msg: c::printbuf,
}
c_default!(bch_read_err_report);

nest! {
    #[derive(CStruct, TypeInfo)]*
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
            #>[derive(Clone, Copy)]
            pub fields: pub struct bch_read_bio_state_fields {
                #[c_bitfield]
                #>[derive(CStruct, TypeInfo)]-
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
        /* its slot on its async object list */
        #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
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

impl c::bch_read_bio {
    pub fn bits(&self) -> c::bch_read_bio_bits {
        // SAFETY: the bits and _state are one u16, any value valid as either
        unsafe { self.state.fields.bits }
    }

    pub fn bits_mut(&mut self) -> &mut c::bch_read_bio_bits {
        // SAFETY: as bits()'s
        unsafe { &mut self.state.fields.bits }
    }

    /// Initializes all but the bio, which is the caller's.
    pub fn init(&mut self, fs: &crate::fs::Fs, opts: c::bch_inode_opts, end_io: c::bio_end_io_t) {
        self.start_time   = crate::util::kernel::local_clock();
        self.c            = fs.as_ptr();
        self.state._state = 0;
        self.flags        = 0;
        self.ret          = 0;
        self.failed       = core::ptr::null_mut();
        self.err_report   = core::ptr::null_mut();
        self.opts         = opts;
        self.bio.bi_end_io = end_io;
        #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
        { self.list_idx = 0; }
    }

    /// Initializes it as a split of @orig - all but the bio, which is the
    /// caller's.
    pub fn init_fragment(&mut self, orig: &mut c::bch_read_bio, failed: *mut c::bch_io_failures) {
        self.c            = orig.c;
        self.state._state = 0;
        self.flags        = 0;
        self.ret          = 0;
        self.bits_mut().set_split(1);
        self.opts         = orig.opts;
        self.failed       = failed;
        self.err_report   = orig.err_report;
        self.completion.parent = core::ptr::from_mut(orig);
        #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
        { self.list_idx = 0; }
    }

    pub fn async_list_del(&mut self, fs: &crate::fs::Fs) {
        #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
        // SAFETY: its slot, from the read path's async_object_list_add()
        unsafe {
            crate::debug::async_objs::list_del(fs, c::bch_async_obj_lists::BCH_ASYNC_OBJ_LIST_rbio, &mut self.list_idx)
        };
        #[cfg(not(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS))]
        let _ = fs;
    }
}

c_extern! {
    #[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
    pub fn bch2_dev_congested_to_text(out: *mut c::printbuf, ca: *mut c::bch_dev);
}

#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
impl c::bch_dev {
    /// How congested its IO is - its latency, against what's expected.
    pub fn congested_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_dev_congested_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}
