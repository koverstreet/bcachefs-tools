// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::c_long;
use cstruct_macros::{bitfield, CStruct};
use nestify::nest;

#[repr(C)]
#[derive(CStruct)]
pub struct dio_read {
    pub cl: c::closure,
    pub req: *mut c::kiocb,
    pub ret: c_long,
    pub should_dirty: bool,
    /*
     * The read's flags. rbio->flags is only set on the rbio the read path
     * picks for an extent - a split, whenever the read bounces - so the
     * dio's own rbio doesn't carry them, and the endio has nowhere else to
     * look.
     */
    pub flags: c::bch_read_flags,
    pub rbio: c::bch_read_bio,
}
c_default!(dio_read);

nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct dio_write {
        pub req: *mut c::kiocb,
        pub mapping: *mut c::address_space,
        pub inode: *mut c::bch_inode_info,
        pub mm: *mut c::mm_struct,
        pub iov: *const c::iovec,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct dio_write_bits {
            #[bits(1)]
            pub loop_: u32,
            #[bits(1)]
            pub extending: u32,
            #[bits(1)]
            pub sync: u32,
            #[bits(1)]
            pub sync_done: u32,
            #[bits(1)]
            pub flush: u32,
            #[bits(3)]
            pub __pad: u8,
        },
        pub quota_res: c::quota_res,
        pub written: u64,

        pub iter: c::iov_iter,
        pub inline_vecs: [c::iovec; 2],

        /* must be last: */
        pub op: c::bch_write_op,
    }
}
c_default!(dio_write);
