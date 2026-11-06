// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_default, rust_only};
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::locking::SeqLock;
use crate::util::ffi::Opaque;
use core::cmp::Ordering;
use core::ffi::c_int;
use cstruct_macros::{bitfield, c_enum, c_export, c_extern, c_xmacro, CStruct, TypeInfo};
use nestify::nest;
use crate::format::{Bpos, Btree};

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
        GC_PHASES(n) => concat(GC_PHASE_, n),
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
            #[bits(8)] pub btree: Btree,
        },
        pub level: u16,
        pub pos: Bpos,
    }
}

/*
 * reflink before the btrees with reflink_ps: gc's table of indirect extents is
 * built walking it, after their repair, and the reflink_ps are counted against
 * it - one a repair left nothing to point to is found missing in the same pass
 */
#[c_export]
pub fn bch2_gc_btree_order(btree: Btree) -> c_int {
    match btree {
        Btree::BTREE_ID_alloc   => -3,
        Btree::BTREE_ID_stripes => -2,
        Btree::BTREE_ID_reflink => -1,
        b                       => b.0 as c_int,
    }
}

impl Ord for gc_pos {
    fn cmp(&self, r: &Self) -> Ordering {
        (self.bits.phase() as u32).cmp(&(r.bits.phase() as u32))
            .then(bch2_gc_btree_order(self.bits.btree()).cmp(&bch2_gc_btree_order(r.bits.btree())))
            .then(self.level.cmp(&r.level))
            .then(self.pos.cmp(&r.pos))
    }
}

impl PartialOrd for gc_pos {
    fn partial_cmp(&self, r: &Self) -> Option<Ordering> {
        Some(self.cmp(r))
    }
}

impl PartialEq for gc_pos {
    fn eq(&self, r: &Self) -> bool {
        self.cmp(r) == Ordering::Equal
    }
}

impl Eq for gc_pos {}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_gc {
    /// How far gc's walk has got: not_running, when there's none.
    pub pos: rust_only!(SeqLock<c::gc_pos>),

    /*
     * The allocation code needs gc_mark in struct bucket to be correct, but
     * it's not while a gc is in progress.
     */
    pub lock: c::rw_semaphore,
}
c_default!(bch_fs_gc);

impl bch_fs_gc {
    pub fn phase(&self) -> c::gc_phase {
        self.pos.read().bits.phase()
    }

    /// Whether gc's walk has got to @pos: gc_visited().
    pub fn visited(&self, pos: c::gc_pos) -> bool {
        pos <= self.pos.read()
    }

    /// gc's walk on to @pos - never back.
    pub fn set_pos(&self, pos: c::gc_pos) {
        let cur = self.pos.read();
        assert!(pos >= cur, "gc position going backwards, from phase {} {} level {} {} to phase {} {} level {} {}",
                cur.bits.phase() as u32, cur.bits.btree(), cur.level, { cur.pos },
                pos.bits.phase() as u32, pos.bits.btree(), pos.level, { pos.pos });
        self.pos.write(|p| *p = pos);
    }

    /// gc's walk done: not_running.
    pub fn set_not_running(&self) {
        self.pos.write(|p| *p = c::gc_pos::default());
    }
}

#[c_export]
pub fn bch2_gc_pos_set(c: &Opaque<Fs>, pos: c::gc_pos) {
    Fs::from_c(c).gc.set_pos(pos)
}

#[c_export]
pub fn bch2_gc_pos_set_not_running(c: &Opaque<Fs>) {
    Fs::from_c(c).gc.set_not_running()
}

#[c_export]
pub fn bch2_gc_phase(c: &Opaque<Fs>) -> c::gc_phase {
    Fs::from_c(c).gc.phase()
}

#[c_export]
pub fn bch2_gc_visited(c: &Opaque<Fs>, pos: c::gc_pos) -> bool {
    Fs::from_c(c).gc.visited(pos)
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
    pub fn bch2_gc_gens(c: *mut Fs) -> c_int;
    pub fn bch2_gc_gens_async(c: *mut Fs);
    pub fn bch2_fs_btree_gc_init_early(c: *mut Fs);
    pub fn bch2_presplit_shard_boundaries(c: *mut Fs) -> c_int;
}

impl Fs {
    pub fn presplit_shard_boundaries(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_presplit_shard_boundaries(self.as_ptr()) })
    }

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
