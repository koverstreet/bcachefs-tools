// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/check_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{GenRadix, c_default};
use cstruct_macros::{bitfield, c_enum, c_typedef, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

c_xmacro! {
    GC_PHASES(x) {
        (not_running),
        (start),
        (sb),
        (btree),
    }
}

macro_rules! __gc_phase_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum gc_phase: u32 {
                $($acc)*
                $([<GC_PHASE_ $n>],)*
            }
        }
    } };
}
GC_PHASES!(__gc_phase_0 []);

#[bitfield(u16, repr = zerocopy::byteorder::native_endian::U16, from = zerocopy::byteorder::native_endian::U16::new, into = zerocopy::byteorder::native_endian::U16::get)]
pub struct gc_pos_phase_bits {
    #[bits(8)]
    pub phase: u32,
    #[bits(8)]
    pub btree: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct gc_pos {
    #[c_anon("")] pub __bitfield_align: [u32; 0],
    #[c_bitfield]
    pub phase_bits: gc_pos_phase_bits,
    pub level: u16,
    pub pos: c::bpos,
}
impl gc_pos {
    pub fn phase(&self) -> u32 { let b = self.phase_bits; b.phase() }
    pub fn set_phase(&mut self, v: u32) { let mut b = self.phase_bits; b.set_phase(v); self.phase_bits = b; }
    pub fn btree(&self) -> u32 { let b = self.phase_bits; b.btree() }
    pub fn set_btree(&mut self, v: u32) { let mut b = self.phase_bits; b.set_btree(v); self.phase_bits = b; }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct reflink_gc {
    pub offset: u64,
    pub size: u32,
    pub refcount: u32,
}

c_typedef! {
    #[c("GENRADIX(struct reflink_gc) reflink_gc_table")]
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
