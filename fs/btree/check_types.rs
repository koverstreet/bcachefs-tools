// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{GenRadix, c_default};
use cstruct_macros::{bitfield, c_enum, c_typedef, c_xmacro, CStruct, TypeInfo};
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
#[derive(Clone, Copy, Default, CStruct)]
pub struct reflink_gc {
    pub offset: u64,
    pub size: u32,
    /* gc's marking and commits' gc triggers count at once */
    pub refcount: c::atomic_t,
}

c_typedef! {
    pub type reflink_gc_table = GenRadix<c::reflink_gc>;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
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
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_gc_gens {
    pub pos: c::bbpos,
    pub work: c::work_struct,
    pub lock: c::mutex,
}
c_default!(bch_fs_gc_gens);
