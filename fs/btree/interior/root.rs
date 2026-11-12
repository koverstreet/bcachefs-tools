// SPDX-License-Identifier: GPL-2.0

//! Btree roots: set by an interior update, read in at bringup, made up fake
//! when one's missing - and each written as a journal entry propagated back
//! to the in-memory roots; and the reserve cache, as text.

use core::ffi::c_ulong;
use core::ptr::{self, NonNull};
use crate::btree::iter::BtreeTrans;
use crate::btree::node::BtreeNode;
use crate::btree::node::sort::build_aux_trees;
use crate::btree::path::{PathIdx, Paths};
use crate::btree::bkey::{BkeyI, BkeyV};
use crate::c;
use crate::data::keylist::KeyRoom;
use crate::debug::trace::events;
use crate::errcode::BchError;
use crate::format::{BkeyType, Btree};
use crate::fs::Fs;
use crate::format::jset_entry;
use crate::prt;
use crate::util::Printbuf;
use crate::util::printbuf::ToText;
use crate::util::vstructs::Unvalidated;
use crate::util::closure::StackClosure;
use crate::{POS_MIN, SPOS_MAX};
use pin_init::stack_pin_init;
use super::{reserve, InteriorUpdate, NewNode};

fn root_pack(b: &BtreeNode) -> c_ulong {
    let mask = (1 << c::BTREE_ROOT_LEVEL_BITS) - 1;
    let p = ptr::from_ref(b) as c_ulong;

    assert!(p & mask == 0, "a btree node not aligned for its level packed with it");
    assert!(c_ulong::from(b.c.level) & !mask == 0, "a root at level {}", b.c.level);
    p | c_ulong::from(b.c.level)
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

    fs.btree.cache.roots.lock().published().roots_b[usize::from(b.c.btree_id)].set(root_pack(b));

    fs.recalc_btree_reserve();
}

impl InteriorUpdate<'_> {
    /// @n made @path's btree's root, its sectors_written stamped.
    pub(crate) fn set_root(&self, paths: &mut Paths<'_>, path: PathIdx, n: &mut NewNode<'_>) {
        let fs = self.fs();

        self.emit_new_node_key(n);

        fs.event_inc_trace(events::btree_node_set_root, |out| prt!(out, "{}", n.pos_to_text(fs)));

        let old = fs.btree_id_root(n.c.btree_id.into()).expect("a btree with no root getting a new one");

        assert!(paths[path].write_locked(old.c.level.into()), "setting a root, the old one not write locked");

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
        assert!(self.btree_id_root(b.c.btree_id.into()).is_none(), "a root read for a btree that has one");

        set_root_inmem(self, b);
    }

    pub fn btree_root_alloc_fake(&self, btree: Btree, level: u32) {
        let trans = crate::btree_trans!(self);
        let _ = crate::btree::trans_attempt::lockrestart_do(&trans, |_| root_alloc_fake_trans(&trans, btree, level));
    }

    /// A btree root as a journal entry has it, propagated back to the
    /// in-memory roots: as each journal write is made.
    pub(crate) fn journal_entry_to_btree_root(&self, entry: &mut jset_entry<Unvalidated>) {
        /*
         * The on-disk root fields (key/level/alive) are owned by root_lock:
         * that's what bch2_btree_set_root_inmem() takes for ->b, and what the
         * journal replay path takes for ->key. Taking interior_updates.lock
         * here was incidental (roots are interior-update machinery), and it
         * dragged interior_updates.lock into the journal write path under
         * sb_lock — closing a lockdep cycle with pin_resize_lock.
         */
        let (btree, level) = (entry.btree_id, entry.level);
        let Some(Ok(mut k)) = entry.entries_mut().next() else { panic!("a btree root entry with no key") };
        let k = unsafe { &*BkeyI::from_raw(k.as_mut_ptr()) };

        let mut g = self.btree.cache.roots.lock();
        let r = g.get_mut(btree.into()).expect("a journalled root of a btree there's no slot for");

        r.level = level;
        r.alive = 1;
        r.key.set_key(k);
    }

    fn btree_alloc_to_text(&self, out: &mut Printbuf, a: &c::btree_alloc) {
        out.indent_add(2);
        prt!(out, "{}\n", a.k.key().to_text(self));

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

    stack_pin_init!(let cl = StackClosure::new());
    loop {
        let ret = trans.btree_cache_cannibalize_lock(NonNull::new(cl.as_ptr()));
        cl.sync();
        if ret.is_ok() {
            break;
        }
    }

    let b = trans.btree_node_mem_alloc(&mut trans.paths(), false);
    trans.btree_cache_cannibalize_unlock();

    let mut b = b?;
    b.set_id(btree, level as usize);
    let n: &BtreeNode = &b;

    n.flags.set_fake(true);
    n.flags.set_need_rewrite(true);

    let contents = unsafe { n.contents_mut() };
    let mut k = KeyRoom::new(contents.key.u64s_mut());
    k.k_mut().set_key_type(BkeyType::btree_ptr);
    k.k_mut().p = SPOS_MAX;
    contents.key.u64s_mut()[crate::btree::bkey::BKEY_U64S] = u64::MAX - u64::from(btree.0);

    n.keys_mut(contents).bset_init_first();
    build_aux_trees(n, contents);

    contents.data_mut().flags = Default::default();
    reserve::set_range(contents, POS_MIN, SPOS_MAX);
    let format = n.calc_format(contents);
    contents.data_mut().format = format;
    contents.set_format(format);

    let state = n.live_state();
    let (n, held) = b.hash(state).unwrap_or_else(|e| panic!("a fake root's transition to live: {e}"));

    set_root_inmem(fs, n);

    drop(held);
    Ok(())
}

