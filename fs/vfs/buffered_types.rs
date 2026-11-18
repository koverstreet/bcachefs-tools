// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use cstruct_macros::{MaybeZeroable, TypeInfo};
use pin_init::pin_data;

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo)]
pub struct bch_writepage_io {
    pub inode: *mut c::InodeInfo,

    /* must be last: */
    #[pin]
    pub op: c::bch_write_op,
}

crate::has_bio!(bch_writepage_io, op.wbio.bio);

// its task's home: its inode is held by its folios' writeback, on whichever
// thread the task runs
unsafe impl Send for c::bch_writepage_io {}
