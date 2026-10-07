// SPDX-License-Identifier: GPL-2.0

//! The data types of vfs/direct_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{bitfield, CStruct};

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(CStruct)]
pub struct dio_read {
    pub cl: c::closure,
    pub req: *mut c::kiocb,
    pub ret: core::ffi::c_long,
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
#[cfg(not(NO_BCACHEFS_FS))]
c_default!(dio_read);

#[cfg(not(NO_BCACHEFS_FS))]
#[bitfield(u8)]
pub struct dio_write_loop_bits {
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
}

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(CStruct)]
pub struct dio_write {
    pub req: *mut c::kiocb,
    pub mapping: *mut c::address_space,
    pub inode: *mut c::bch_inode_info,
    pub mm: *mut c::mm_struct,
    pub iov: *const c::iovec,
    #[c_bitfield]
    pub loop_bits: dio_write_loop_bits,
    pub quota_res: c::quota_res,
    pub written: u64,

    pub iter: c::iov_iter,
    pub inline_vecs: [c::iovec; 2],

    /* must be last: */
    pub op: c::bch_write_op,
}
#[cfg(not(NO_BCACHEFS_FS))]
c_default!(dio_write);
#[cfg(not(NO_BCACHEFS_FS))]
impl dio_write {
    pub fn loop_(&self) -> u32 { let b = self.loop_bits; b.loop_() }
    pub fn set_loop_(&mut self, v: u32) { let mut b = self.loop_bits; b.set_loop_(v); self.loop_bits = b; }
    pub fn extending(&self) -> u32 { let b = self.loop_bits; b.extending() }
    pub fn set_extending(&mut self, v: u32) { let mut b = self.loop_bits; b.set_extending(v); self.loop_bits = b; }
    pub fn sync(&self) -> u32 { let b = self.loop_bits; b.sync() }
    pub fn set_sync(&mut self, v: u32) { let mut b = self.loop_bits; b.set_sync(v); self.loop_bits = b; }
    pub fn sync_done(&self) -> u32 { let b = self.loop_bits; b.sync_done() }
    pub fn set_sync_done(&mut self, v: u32) { let mut b = self.loop_bits; b.set_sync_done(v); self.loop_bits = b; }
    pub fn flush(&self) -> u32 { let b = self.loop_bits; b.flush() }
    pub fn set_flush(&mut self, v: u32) { let mut b = self.loop_bits; b.set_flush(v); self.loop_bits = b; }
}
