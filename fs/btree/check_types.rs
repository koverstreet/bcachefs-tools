// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_default};
use crate::util::projections::Volatile;
use crate::locking::{Locked, SeqLock};
use crate::new_lock;
use core::cmp::Ordering;
use cstruct_macros::{bitfield, bch_enum, CStruct, Init, TypeInfo};
use pin_init::pin_data;
use nestify::nest;
use crate::format::{Bpos, Btree};

#[bch_enum(closed)]
#[prefix(GC_PHASE_)]
#[repr(u32)]
pub enum GcPhase {
    not_running,
    start,
    sb,
    btree,
}

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct)]
    pub struct gc_pos {
        #[c_bitfield]
        #>[bitfield(u16)]
        pub bits: pub struct gc_pos_bits {
            #[bits(8)] pub phase: c::GcPhase,
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
pub fn gc_btree_order(btree: Btree) -> i32 {
    match btree {
        Btree::alloc   => -3,
        Btree::stripes => -2,
        Btree::reflink => -1,
        b              => b.0 as i32,
    }
}

/*
 * For concurrent mark and sweep (with other index updates), we define a total
 * ordering of _all_ references GC walks:
 *
 * Note that some references will have the same GC position as others - e.g.
 * everything within the same btree node; in those cases we're relying on
 * whatever locking exists for where those references live, i.e. the write lock
 * on a btree node.
 *
 * That locking is also required to ensure GC doesn't pass the updater in
 * between the updater adding/removing the reference and updating the GC marks;
 * without that, we would at best double count sometimes.
 *
 * That part is important - whenever calling bch2_mark_pointers(), a lock _must_
 * be held that prevents GC from passing the position the updater is at.
 *
 * (What about the start of gc, when we're clearing all the marks? GC clears the
 * mark with the gc pos seqlock held, and bch_mark_bucket checks against the gc
 * position inside its cmpxchg loop, so crap magically works).
 */
impl gc_pos {
    /* Position of (the start of) a gc phase: */
    pub fn of_phase(phase: c::GcPhase) -> Self {
        Self { bits: c::gc_pos_bits::new().with_phase(phase), ..Default::default() }
    }

    /// gc_pos_btree(): @pos at @level of @btree, in gc's walk of the btrees.
    pub fn in_btree(btree: Btree, level: u16, pos: Bpos) -> Self {
        Self {
            bits: c::gc_pos_bits::new().with_phase(c::GcPhase::btree).with_btree(btree),
            level,
            pos,
        }
    }
}

impl Ord for gc_pos {
    fn cmp(&self, r: &Self) -> Ordering {
        (self.bits.phase() as u32).cmp(&(r.bits.phase() as u32))
            .then(gc_btree_order(self.bits.btree()).cmp(&gc_btree_order(r.bits.btree())))
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

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_gc {
    /// How far gc's walk has got: not_running, when there's none.
    pub pos: SeqLock<c::gc_pos>,

    /*
     * The allocation code needs gc_mark in struct bucket to be correct, but
     * it's not while a gc is in progress.
     */
    #[pin]
    #[init(new_lock!((), "&c->gc.lock"))]
    pub lock: Locked<c::rw_semaphore>,
}

impl bch_fs_gc {
    pub fn phase(&self) -> c::GcPhase {
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

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_gc_gens {
    /* gc_gens's: where its walk is, for sysfs */
    pub pos: Volatile<c::bbpos>,
}
c_default!(bch_fs_gc_gens);
