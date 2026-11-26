// SPDX-License-Identifier: GPL-2.0

//! Btree roots: set by an interior update, read in at bringup, made up fake
//! when one's missing - and each written as a journal entry propagated back
//! to the in-memory roots; and the reserve cache, as text.

use core::ptr;
use crate::btree::iter::BtreeTrans;
use crate::btree::node::BtreeNode;
use crate::btree::node::sort::build_aux_trees;
use crate::btree::path::{PathIdx, Paths};
use crate::btree::bkey::{BkeyV, BkeyI, BKEY_U64S};
use crate::btree::interior::BtreeAlloc;
use crate::data::extents_format::BKEY_BTREE_PTR_VAL_U64s_MAX;
use crate::debug::trace::events;
use crate::errcode::BchError;
use crate::format::{BkeyType, Btree, BTREE_ID_NR_MAX, JsetEntry, jset_entry_btree_root};
use crate::fs::Fs;
use crate::{prt, POS_MIN, SPOS_MAX};
use crate::util::Printbuf;
use crate::util::printbuf::ToText;
use crate::util::vstructs::Validated;
use super::{reserve, InteriorUpdate, NewNode};
use crate::data_structures::dst_room::DstRoom;
use crate::locking::{ExclusiveGuard, Mutex};
use crate::util::alloc::Vec;
use crate::util::projections::Volatile;
use cstruct_macros::{Init, MaybeZeroable};
use pin_init::pin_data;

/* On disk root - see async splits: */
#[repr(C)]
#[derive(Default)]
pub struct BtreeRoot {
    pub key: DstRoom<BkeyI, [u64; BKEY_U64S + BKEY_BTREE_PTR_VAL_U64s_MAX]>,
    pub level: u8,
    pub alive: u8,
}

/// root_lock's: each btree's root as the journal and the clean section
/// record it - key, level, alive. The known btrees', and the rest's, which
/// early journal replay adds as it finds them: the slots only move then.
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct BtreeRoots {
    #[init(::pin_init::init_array_from_fn(|_| BtreeRoot::default()))]
    pub roots_known: [BtreeRoot; Btree::NR as usize],
    #[pin]
    #[init(Vec::new())]
    pub roots_extra: Vec<BtreeRoot>,
}

impl BtreeRoots {
    /// Btree @id's: a known btree's, or past them, roots_extra's -
    /// bch2_btree_id_root(). None for one past those.
    pub fn get(&self, id: u32) -> Option<&BtreeRoot> {
        let nr_known = Btree::NR;
        if id < nr_known {
            return Some(&self.roots_known[id as usize]);
        }
        self.roots_extra.get((id - nr_known) as usize)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut BtreeRoot> {
        let nr_known = Btree::NR;
        if id < nr_known {
            return Some(&mut self.roots_known[id as usize]);
        }
        self.roots_extra.get_mut((id - nr_known) as usize)
    }
}

/// root_lock, held: the roots, and what it publishes of them.
pub type BtreeRootsGuard<'a> = ExclusiveGuard<'a, Mutex, BtreeRoots, BtreeRootsLive>;

impl BtreeRootsGuard<'_> {
    /// Slots up to btree @id's, empty, and how many there are published: for
    /// a btree past the known ones that the journal has a root for.
    pub fn grow_to(&mut self, id: u32) -> Result<(), BchError> {
        while id >= self.roots_extra.len() as u32 + Btree::NR {
            self.roots_extra.push(Default::default())?;
        }
        let nr = self.roots_extra.len() as u32;
        self.published().roots_nr_extra.set(nr);
        Ok(())
    }
}

/* A packed root's level, in its node pointer's low bits */
pub const BTREE_ROOT_LEVEL_BITS: u32 = 3;

crate::published! {
    /// The roots as they're read without root_lock, by btree id: the root
    /// node, packed with its level - so btree_path_lock_root() reads both in
    /// one load, without a miss into the node - and the error reading it
    /// left; and how many btrees there are past the known ones. Fixed at
    /// BTREE_ID_NR_MAX, so nothing here moves when roots_extra grows.
    #[derive(MaybeZeroable)]
    pub struct BtreeRootsLive {
        pub roots_b: [Volatile<usize>; BTREE_ID_NR_MAX as usize],
        pub roots_error: [Volatile<i16>; BTREE_ID_NR_MAX as usize],
        pub roots_nr_extra: Volatile<u32>,
    }
}

impl BtreeRootsLive {
    /// Btree @id's root node: bch2_btree_id_root_b().
    pub fn node(&self, id: u32) -> Option<&BtreeNode> {
        let b = self.roots_b.get(id as usize)?.get() & !((1 << BTREE_ROOT_LEVEL_BITS) - 1);
        // SAFETY: a root's node, live while it's the root
        unsafe { (b as *const BtreeNode).as_ref() }
    }

    /// The error reading btree @id's root left.
    pub fn error(&self, id: u32) -> i32 {
        self.roots_error.get(id as usize).map_or(0, |e| e.get().into())
    }
}


fn root_pack(b: &BtreeNode) -> usize {
    let mask = (1 << BTREE_ROOT_LEVEL_BITS) - 1;
    let p = ptr::from_ref(b) as usize;

    assert!(p & mask == 0, "a btree node not aligned for its level packed with it");
    assert!(usize::from(b.c.level.get()) & !mask == 0, "a root at level {}", b.c.level.get());
    p | usize::from(b.c.level.get())
}

fn set_root_inmem(fs: &Fs, b: &BtreeNode) {
    /*
     * Root nodes cannot be reaped. The flag (rather than off-list
     * placement) is the mechanism: the reclaim path checks
     * btree_node_permanent and skips. Roots stay on bc->list,
     * counted normally; cache_exit's teardown sweep finds them.
     */
    {
        let _g = fs.btree.cache.lock.lock();
        b.flags.set_permanent(true);
    }

    fs.btree.cache.roots.lock().published().roots_b[usize::from(b.c.btree_id.get())].set(root_pack(b));

    fs.recalc_btree_reserve();
}

impl InteriorUpdate<'_> {
    /// @n made @path's btree's root, its sectors_written stamped.
    pub(crate) fn set_root(&self, paths: &mut Paths<'_>, path: PathIdx, n: &mut NewNode<'_>) {
        let fs = self.fs();

        self.emit_new_node_key(n);

        fs.event_inc_trace(events::btree_node_set_root, |out| prt!(out, "{}", n.pos_to_text(fs)));

        let old = fs.btree_id_root(n.c.btree_id.get().into()).expect("a btree with no root getting a new one");

        assert!(paths[path].write_locked(old.c.level.get().into()), "setting a root, the old one not write locked");

        set_root_inmem(fs, n);

        self.updated_root();
    }
}

impl Fs {
    /*
     * Only for filesystem bringup, when first reading the btree roots or allocating
     * btree roots when initializing a new filesystem:
     */
    pub(crate) fn btree_set_root_for_read(&self, b: &BtreeNode) {
        assert!(self.btree_id_root(b.c.btree_id.get().into()).is_none(), "a root read for a btree that has one");

        set_root_inmem(self, b);
    }

    pub fn btree_root_alloc_fake(&self, btree: Btree, level: u32) {
        let trans = crate::btree_trans!(self);
        let _ = crate::btree::trans_attempt::lockrestart_do(&trans, |_| root_alloc_fake_trans(&trans, btree, level));
    }

    /// A btree root as journal entry @e has it, propagated back to the
    /// in-memory roots: as each journal write is made.
    pub(crate) fn journal_entry_to_btree_root(&self, e: &JsetEntry) {
        /*
         * The on-disk root fields (key/level/alive) are owned by root_lock:
         * that's what bch2_btree_set_root_inmem() takes for ->b, and what the
         * journal replay path takes for ->key. Taking interior_updates.lock
         * here was incidental (roots are interior-update machinery), and it
         * dragged interior_updates.lock into the journal write path under
         * sb_lock — closing a lockdep cycle with pin_resize_lock.
         */
        let root = e.arm::<jset_entry_btree_root<Validated>>().expect("a btree root entry");
        let k = root.entries().next().expect("a btree root entry with no key");

        let mut g = self.btree.cache.roots.lock();
        let r = g.get_mut(e.btree_id.into()).expect("a journalled root of a btree there's no slot for");

        r.level = e.level;
        r.alive = 1;
        r.key.set_key(k);
    }

    fn btree_alloc_to_text(&self, out: &mut Printbuf, a: &BtreeAlloc) {
        out.indent_add(2);
        prt!(out, "{}\n", (&*a.k).to_text(self));

        for ob in a.ob.iter(self) {
            ob.to_text(out);
        }
        out.indent_sub(2);
    }

    #[cold]
    pub fn btree_reserve_cache_to_text(&self, out: &mut Printbuf) {
        let cache = self.btree.reserve_cache.lock.lock();
        for a in &cache.data[..cache.nr as usize] {
            self.btree_alloc_to_text(out, a);
        }
    }
}

pub(crate) fn root_alloc_fake_trans(trans: &BtreeTrans<'_>, btree: Btree, level: u32) -> Result<(), BchError> {
    let fs = trans.fs();

    trans.btree_cache_cannibalize_lock();

    let b = trans.btree_node_mem_alloc(&mut trans.paths(), false);
    trans.btree_cache_cannibalize_unlock();

    let mut b = b?;
    b.set_id(btree, level as usize);

    b.flags.set_fake(true);
    b.flags.set_need_rewrite(true);

    let mut locked = b.locked();
    let n = locked.node();
    locked.update_key(|room| {
        let k = room.init();
        k.k.set_key_type(BkeyType::btree_ptr);
        k.k.p = SPOS_MAX;
        room.elems_mut()[crate::btree::bkey::BKEY_U64S] = u64::MAX - u64::from(btree.0);
    });

    let contents = locked.contents_mut();
    n.keys_mut(contents).bset_init_first();
    build_aux_trees(n, contents);

    contents.data_mut().flags = Default::default();
    reserve::set_range(n, contents, POS_MIN, SPOS_MAX);
    let format = n.calc_format(contents);
    contents.data_mut().format = format;
    contents.set_format(format);

    let state = b.live_state();
    let (n, held) = b.hash(fs, state).unwrap_or_else(|e| panic!("a fake root's transition to live: {e}")).into_held(fs);

    set_root_inmem(fs, n);

    drop(held);
    Ok(())
}

