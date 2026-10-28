// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use core::ffi::c_int;
use cstruct_macros::{bitfield, c_enum, c_extern, c_xmacro, CStruct, TypeInfo};
use nestify::nest;

c_xmacro! {
    GC_PHASES(x) {
        (not_running),
        (start),
        (sb),
        (btree),
    }
}

c_enum! {
    #[closed]
    pub enum gc_phase: u32 {
        GC_PHASES(n) => GC_PHASE_ ## n,
    }
}

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct)]
    pub struct gc_pos {
        #[c_bitfield]
        #>[bitfield(u16)]
        pub bits: pub struct gc_pos_bits {
            #[bits(8)] pub phase: c::gc_phase,
            #[bits(8)] pub btree: c::btree_id,
        },
        pub level: u16,
        pub pos: c::bpos,
    }
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_gc {
    pub pos_lock: c::seqcount_t,
    pub pos: c::gc_pos,

    /*
     * The allocation code needs gc_mark in struct bucket to be correct, but
     * it's not while a gc is in progress.
     */
    pub lock: c::rw_semaphore,
}
c_default!(bch_fs_gc);

impl bch_fs_gc {
    /// Its phase - not_running, unless a gc is walking - read once, without
    /// pos_lock: as C's reads of c->gc.pos.phase.
    pub fn phase(&self) -> c::gc_phase {
        // SAFETY: read once, volatile, as C's plain reads
        unsafe { core::ptr::read_volatile(&self.pos.bits) }.phase()
    }
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_gc_gens {
    pub pos: c::bbpos,
    pub work: c::work_struct,
    pub lock: c::mutex,
}
c_default!(bch_fs_gc_gens);

c_extern! {
    pub fn bch2_gc_gens(c: *mut c::bch_fs) -> c_int;
    pub fn bch2_gc_gens_async(c: *mut c::bch_fs);
}

impl Fs {
    /// Recalculate each bucket's oldest gen, so gens can wrap.
    pub fn gc_gens(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_gc_gens(self.as_ptr()) })
    }

    /// gc_gens(), from its work, holding a write ref - unless the
    /// filesystem is going read-only.
    pub fn gc_gens_async(&self) {
        unsafe { bch2_gc_gens_async(self.as_ptr()) }
    }
}
