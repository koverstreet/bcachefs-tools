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
}

impl Fs {
    /// Recalculate each bucket's oldest gen, so gens can wrap.
    pub fn gc_gens(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_gc_gens(self.as_ptr()) })
    }
}
