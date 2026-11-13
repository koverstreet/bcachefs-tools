// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use cstruct_macros::{bitfield, CStruct};
use nestify::nest;

nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct dio_write {
        pub req: *mut c::kiocb,
        pub mapping: crate::vfs::folio::Mapping,
        pub inode: *mut c::bch_inode_info,
        pub mm: Option<crate::util::kernel::Mm>,
        pub iov: *const c::iovec,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct dio_write_bits {
            loop_:     u1,
            extending: u1,
            sync:      u1,
            sync_done: u1,
            flush:     u1,
            __pad:     u3,
        },
        pub quota_res: c::quota_res,
        pub written: u64,

        pub iter: c::iov_iter,
        pub inline_vecs: [c::iovec; 2],

        /* the dio count and the page cache blocked, and c->writes: till it's
         * done */
        pub started: Option<crate::vfs::inode::DioStarted>,
        pub write_ref: Option<crate::util::refcount::RefGuard<crate::util::refcount::PercpuRef>>,

        /* must be last: */
        pub op: c::bch_write_op,
    }
}

crate::has_bio!(dio_write, op.wbio.bio);
