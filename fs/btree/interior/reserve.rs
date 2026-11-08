// SPDX-License-Identifier: GPL-2.0

//! An interior update's nodes: its reserve, allocated before it changes
//! anything - the worst case, splitting up to a new root - and given back when
//! it's done, the space of those it didn't use to the reserve cache; the nodes
//! it takes from it, set up; and the nodes it frees, freed in memory.

use core::mem::size_of;
use core::ptr::{self, NonNull};
use crate::alloc::capacity::DiskReservation;
use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{BkeySC, BkeyV, BkeyVMut};
use crate::btree::iter::CommitOpts;
use crate::btree::locking::bch2_btree_cache_cannibalize_unlock;
use crate::btree::node::BtreeNode;
use crate::btree::node::bset::bch2_bset_init_first;
use crate::btree::node::sort::{bch2_btree_build_aux_trees, bch2_btree_sort_into};
use crate::btree::path::{LockedNode, PathIdx, Paths};
use crate::btree::read::{bch2_btree_node_mem_alloc, bch2_btree_node_transition_state};
use crate::btree::traverse::bch2_btree_cache_cannibalize_lock;
use crate::c;
use crate::data::durability::BkeyVDurability;
use crate::data::keylist::KeyRoom;
use crate::data::ptrs::{BkeyVPtrs, BtreePtrs};
use crate::data_structures::array_vec::ArrayVec;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, errptr_to_result, ret_to_result_void, BchError};
use crate::{POS_MIN, SPOS_MAX};
use crate::format::{BkeyType, Bpos, DataType};
use crate::fs::{writepoint_ptr, Fs};
use crate::locking::six::{six_lock_increment_ip, six_unlock_ip, SixHold};
use crate::prt;
use crate::util::closure::StackClosure;
use crate::util::kernel::WRITE;
use crate::util::rcu::read_lock;
use crate::util::vstructs::Head;
use pin_init::stack_pin_init;
use super::{BtreeUpdate, InteriorUpdate, NewNode, BTREE_RESERVE_MAX};

/**
 * bch2_btree_node_format_fits - check if we could rewrite node with a new format
 *
 * @c:		filesystem handle
 * @b:		btree node to rewrite
 * @nr:		number of keys for new node (i.e. b->nr)
 * @new_f:	bkey format to translate keys to
 *
 * Returns: true if all re-packed keys will be able to fit in a new node.
 *
 * Assumes all keys will successfully pack with the new format.
 */
fn format_fits(b: &BtreeNode, new_f: &c::bkey_format) -> bool {
    let contents = unsafe { b.contents() };
    let u64s = contents.nr.u64s_with_format(&contents.format, new_f);

    size_of::<Head<c::btree_node>>() + u64s * size_of::<u64>() < b.buf_bytes()
}

/* Btree node freeing/allocation: */

fn node_free(fs: &Fs, b: &BtreeNode) {
    fs.event_inc_trace(events::btree_node_free, |out| prt!(out, "{}", b.pos_to_text(fs)));

    assert!(!b.flags.write_blocked(), "freeing a write blocked node");
    assert!(!b.flags.need_write(), "freeing a node that needs writing");
    assert!(!fs.btree_id_root(b.c.btree_id.into()).is_some_and(|r| ptr::eq(r, b)), "freeing a root");
    assert!(b.ob.nr == 0, "freeing a node holding open buckets");
    assert!(unsafe { (*b.write_blocked.data_ptr()).is_empty() }, "freeing a node interior updates are blocking");

    b.flags.set_permanent(false);
    b.flags.set_noevict(false);
}

fn transition_state(fs: &Fs, b: &BtreeNode, state: c::btree_node_cache_state) -> core::ffi::c_int {
    unsafe {
        bch2_btree_node_transition_state((&raw const fs.btree.cache).cast_mut(), ptr::from_ref(b).cast_mut(), state)
    }
}

pub(super) fn btree_set_min(contents: &mut crate::btree::node::NodeContents, pos: Bpos) {
    if let Some(mut p) = contents.key.key_mut().as_s().arm::<c::BtreePtrV2>() {
        p.v_mut().min_key = pos;
    }
    contents.data_mut().min_key = pos;
}

pub(super) fn btree_set_max(contents: &mut crate::btree::node::NodeContents, pos: Bpos) {
    contents.key.key_mut().k.p = pos;
    contents.data_mut().max_key = pos;
}

fn can_use_btree_node(fs: &Fs, res: &DiskReservation<'_>, target: u32, k: BkeySC<'_>) -> bool {
    let k = k.arm::<BtreePtrs>().expect("a btree node pointer");

    if !k.devs_rw(fs) {
        return false;
    }

    if target != 0 && !k.in_target(fs, Target::decode(target)) {
        return false;
    }

    let durability = u32::from(k.btree_ptr_durability(fs).online);

    if durability >= res.nr_replicas.get() {
        return true;
    }

    let devs = fs.target_rw_devs(DataType::btree, Target::decode(target));
    let rcu = read_lock();
    let available: u32 = devs.d.iter()
        .filter_map(|i| fs.dev_rcu(i as u32, &rcu))
        .map(|ca| ca.rw_durability())
        .sum();

    durability >= available
}

impl InteriorUpdate<'_> {
    /// The disk space, and the nodes, leaf and interior, that @nr_nodes says
    /// the update might need, reserved.
    pub(super) fn reserve(&self, nr_nodes: [usize; 2], target: u32, commit: CommitOpts,
                          write: c::bch_write_flags) -> Result<(), BchError> {
        use c::bch_watermark as wm;

        let trans = self.trans;
        let fs = self.fs();
        let watermark = commit.get_watermark();
        let flags = commit.to_c();
        let disk_res_flags = if flags.contains(c::bch_trans_commit_flags::no_enospc) {
            c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL
        } else {
            c::bch_reservation_flags::empty()
        };

        self.disk_res.add((nr_nodes[0] + nr_nodes[1]) as u64 * u64::from(fs.btree_sectors()),
                          u32::from(fs.opts().metadata_replicas.get()), disk_res_flags)?;

        let opts = fs.opts();
        let target = [target, opts.metadata_target.get().into(), opts.foreground_target.get().into()]
            .into_iter().find(|&t| t != 0).unwrap_or(0);
        let mut devs_have = c::bch_devs_list::default();
        let nr_replicas = self.disk_res.nr_replicas.get();
        let t = unsafe { trans.attempt_in_progress() };
        let mut req = t.alloc_request(target, false, &mut devs_have, nr_replicas, nr_replicas,
                                      watermark, write, ptr::null_mut())?;

        let ret = match self.reserve_get(nr_nodes, &mut req) {
            Err(e) if e.matches(c::EAGAIN) || e.matches(c::ENOMEM) => {
                /*
                 * XXX: this should probably be a separate BTREE_INSERT_NONBLOCK
                 * flag
                 */
                if e.matches(c::EAGAIN) &&
                   flags.contains(c::bch_trans_commit_flags::journal_reclaim) &&
                   (watermark as u32) < wm::reclaim as u32 {
                    return fs.throw(bch_errcode::BCH_ERR_journal_reclaim_would_deadlock);
                }

                stack_pin_init!(let cl = StackClosure::new());
                req.cl = cl.as_ptr();

                let ret = loop {
                    match self.reserve_get(nr_nodes, &mut req) {
                        Err(e) if e.matches(bch_errcode::BCH_ERR_operation_blocked) => {
                            if let Err(e) = t.wait_on_allocator(&mut req, e, cl.as_opaque()) {
                                break Err(e);
                            }
                        }
                        r => break r,
                    }
                };

                /*
                 * Don't block with btree locks held
                 *
                 * It would be nice if we could remove closures from waitlists
                 * without waking up the waitlist:
                 */
                if cl.nr_remaining() > 1 {
                    trans.unlock();
                }
                ret
            }
            r => r,
        };

        if let Err(e) = ret {
            fs.event_inc_trace(events::btree_reserve_get_fail, |out| {
                prt!(out, "{}\n", trans.fn_name());
                prt!(out, "need {} ret {}\n", nr_nodes[0] + nr_nodes[1], e.msg());
            });
        }
        ret
    }

    fn reserve_get(&self, nr_nodes: [usize; 2], req: &mut c::alloc_request) -> Result<(), BchError> {
        let trans = self.trans;

        assert!(nr_nodes[0] + nr_nodes[1] <= BTREE_RESERVE_MAX as usize,
                "an interior update reserving {} nodes", nr_nodes[0] + nr_nodes[1]);

        /*
         * Protects reaping from the btree node cache and using the btree node
         * open bucket reserve:
         */
        ret_to_result_void(unsafe { bch2_btree_cache_cannibalize_lock(trans.raw(), req.cl) })?;

        let ret = (|| {
            for (interior, &nr) in nr_nodes.iter().enumerate() {
                while self.owned.borrow().reserve[interior].len() < nr {
                    let b = self.node_alloc_from_allocator(interior != 0, req)?;
                    self.owned.borrow_mut().reserve[interior].push(b);
                }
            }
            Ok(())
        })();

        unsafe { bch2_btree_cache_cannibalize_unlock(trans.raw()) };
        ret
    }

    fn node_alloc_from_allocator(&self, interior: bool, req: &mut c::alloc_request)
        -> Result<NonNull<BtreeNode>, BchError>
    {
        let trans = self.trans;
        let fs = self.fs();

        let b = errptr_to_result(NonNull::new(unsafe { bch2_btree_node_mem_alloc(trans.raw(), interior) })
            .expect("btree_node_mem_alloc() returns a node or an error"))?;
        let n = unsafe { b.as_ref() };

        assert!(n.ob.nr == 0, "a node from the cache holding open buckets");

        let ret = (|| {
            let t = unsafe { trans.attempt_in_progress() };
            let mut wp = loop {
                let wp = t.alloc_sectors(req, writepoint_ptr(&fs.allocator.btree_write_point))?;
                if wp.sectors_free() >= fs.btree_sectors() {
                    break wp;
                }
                wp.retire_short(fs.btree_sectors());
            };

            let contents = unsafe { n.contents_mut() };
            let ob = unsafe { &mut (*b.as_ptr()).ob };

            let mut cache = fs.btree.reserve_cache.lock.lock();
            while cache.nr != 0 {
                cache.nr -= 1;
                let nr = cache.nr as usize;
                let a = &mut cache.data[nr];

                /* check if it has sufficient durability */

                let target = if req.flags.contains(c::bch_write_flags::only_specified_devs) {
                    req.target
                } else {
                    0
                };
                if can_use_btree_node(fs, &self.disk_res, target, BkeySC::from(a.k.key())) {
                    contents.key.set_key(a.k.key());
                    *ob = core::mem::take(&mut a.ob);
                    return Ok(());
                }

                a.ob.drain(fs).for_each(drop);
            }
            drop(cache);

            let mut k = KeyRoom::new(contents.key.u64s_mut());
            k.k_mut().set_key_type(BkeyType::btree_ptr_v2);
            k.set_val_u64s(size_of::<c::BtreePtrV2>() / size_of::<u64>())
                .expect("a btree node's key with room for a btree_ptr_v2");
            wp.append_ptrs(&mut k, fs.btree_sectors(), false);

            wp.open_buckets_get(ob);
            Ok(())
        })();

        /*
         * Leave both intent and write held: nodes on the prealloc list stay
         * locked (they aren't reachable to any other trans — not yet hashed
         * in, or freshly hash-removed — so holding write blocks nothing).
         * Consumers can use them without any lock-take at pop time.
         */
        if let Err(e) = ret {
            transition_state(fs, n, c::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE);
            unsafe {
                six_unlock_ip(n.c.lock.six(), c::six_lock_type::SIX_LOCK_write, crate::ret_ip!());
                six_unlock_ip(n.c.lock.six(), c::six_lock_type::SIX_LOCK_intent, crate::ret_ip!());
            }
            return Err(e);
        }
        Ok(b)
    }

    pub(crate) fn alloc_node(&self, _paths: &mut Paths<'_>, level: usize) -> NewNode<'_> {
        let fs = self.fs();

        assert!(level < c::BTREE_MAX_DEPTH as usize, "allocating a node at level {level}");

        let b = {
            let mut o = self.owned.borrow_mut();
            let b = o.reserve[usize::from(level != 0)].pop().expect("an interior update's reserve, used up");
            o.consumed.push(b);
            b
        };

        /*
         * @as keeps owning intent+write refs on @b until reserve_put runs at
         * update_done/update_free time.  Consumers (this function's callers)
         * additionally take their own intent+write recurses through the path
         * machinery at the WRITE_LOCKED mark site.
         */

        let n = unsafe { b.as_ref() };
        n.flags.set_accessed(true);

        let contents = unsafe { n.contents_mut() };
        bch2_bset_init_first(n, (&raw mut contents.data_mut().keys).cast());
        unsafe {
            (*b.as_ptr()).c.level    = level as u8;
            (*b.as_ptr()).c.btree_id = self.btree.0 as u8;
        }
        contents.version_ondisk = fs.sb.version.get();

        contents.nr = Default::default();
        let data = contents.data_mut();
        data.magic = crate::types::Le64::new(fs.bset_magic());
        data._ptr = Default::default();
        data.flags = Default::default();
        data.set_id_lo(self.btree.0 as u64 & 15);
        data.set_id_hi(self.btree.0 as u64 >> 4);
        data.set_level(level as u64);
        let seq = data.keys.seq;

        if let Some(mut p) = contents.key.key_mut().as_s().arm::<c::BtreePtrV2>() {
            let v = p.v_mut();
            v.mem_ptr         = 0;
            v.seq             = seq;
            v.sectors_written = Default::default();
        }

        contents.data_mut().set_new_extent_overwrite(true);

        bch2_btree_build_aux_trees(n);

        assert!(transition_state(fs, n, n.live_state()) == 0, "a new node's transition to live");

        fs.event_inc_trace(events::btree_node_alloc, |out| prt!(out, "{}", n.pos_to_text(fs)));
        fs.io_clock[WRITE].increment(fs.btree_sectors().into());
        self.new_node(b)
    }

    /// A copy of @b, compacted.
    pub(crate) fn alloc_replacement(&self, paths: &mut Paths<'_>, b: &BtreeNode) -> NewNode<'_> {
        let fs = self.fs();
        let mut n = self.alloc_node(paths, b.c.level.into());
        let b_contents = unsafe { b.contents() };
        let mut format = b.calc_format(b_contents);

        /*
         * The keys might expand with the new format - if they wouldn't fit in
         * the btree node anymore, use the old format for now:
         */
        if !format_fits(b, &format) {
            format = b_contents.format;
        }

        let contents = n.contents_mut();
        contents.data_mut().set_seq(b_contents.data().seq() + 1);

        btree_set_min(contents, b_contents.data().min_key);
        btree_set_max(contents, b_contents.data().max_key);

        contents.data_mut().format = format;
        contents.set_format(format);

        bch2_btree_sort_into(fs, &n, b);

        n.contents().reset_sib_u64s();
        n
    }

    /// A new root at @level, @path holding it above what it held, with no
    /// siblings to merge with.
    pub(crate) fn alloc_root(&self, paths: &mut Paths<'_>, path: PathIdx, level: usize) -> NewNode<'_> {
        let mut n = self.alloc_node(paths, level);

        let contents = n.contents_mut();
        btree_set_min(contents, POS_MIN);
        btree_set_max(contents, SPOS_MAX);
        let format = n.calc_format(n.contents());

        let contents = n.contents_mut();
        contents.data_mut().format = format;
        contents.set_format(format);
        bch2_btree_build_aux_trees(&n);

        let locks_want = paths[path].locks_want();
        paths[path].bits.set_locks_want(locks_want + 1);
        assert!(!paths[path].locked(level), "a new root's level already locked");
        self.take_node(paths, path, &n);

        for sib in &n.contents().sib_u64s {
            sib.set(u16::MAX);
        }
        n
    }

    /// A path to @n, holding it.
    pub(crate) fn take_path(&self, paths: &mut Paths<'_>, n: &NewNode<'_>) -> PathIdx {
        let pos = n.contents().key_i().k().p;
        let idx = self.trans.path_get_unlocked_mut(paths, crate::format::Btree(n.c.btree_id.into()),
                                                   n.c.level.into(), pos, false, crate::ret_ip!());
        self.take_node(paths, idx, n);
        idx
    }

    /*
     * Install a freshly-consumed prealloc node on @path at WRITE_LOCKED.
     *
     * Path takes its own intent+write recurses on top of the as-owned primary
     * refs from bch2_btree_node_mem_alloc — that keeps the path-state-vs-lock-
     * count invariant true.  six_lock_increment(SIX_LOCK_write) bumps both
     * write_lock_recurse and (via fallthrough) intent_lock_recurse — one call
     * gives both recurses.
     *
     * When the path eventually unlocks (trans teardown or explicit
     * bch2_path_put), btree_node_unlock for WRITE_LOCKED drops one intent +
     * one write balanced against the recurses we take here; the as-owned
     * refs survive and are dropped by bch2_btree_update_done (success) or
     * bch2_btree_reserve_put (err).
     */
    fn take_node(&self, paths: &mut Paths<'_>, path: PathIdx, n: &NewNode<'_>) {
        let level = usize::from(n.c.level);
        let hold = unsafe {
            six_lock_increment_ip(n.c.lock.six(), c::six_lock_type::SIX_LOCK_write, 0);
            SixHold::from_raw(&n.c.lock, c::six_lock_type::SIX_LOCK_write)
        };
        paths[path].put_level(level, &n.c, hold);
        paths[path].lock_taken(level);
        self.trans.path_level_init(paths, path, level, n.locked());
    }

    /// @b, freed by the update, freed in memory: @path holds it write locked.
    pub(super) fn node_free_inmem(&self, paths: &mut Paths<'_>, path: PathIdx, b: LockedNode<'_, BtreeNode>) {
        let fs = self.fs();

        assert!(paths[path].write_locked(b.c.level.into()), "freeing a node not write locked");

        node_free(fs, &b);

        transition_state(fs, &b, c::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE);
        self.trans.node_forget(paths, &b);
    }
}

impl BtreeUpdate {
    /// The update's reserve given back - the nodes it didn't take, and on
    /// rollback, @bailed, those it did - their space kept in the reserve
    /// cache, and every reserved node's locks dropped.
    pub(super) fn reserve_put(&self, trans: &crate::btree::iter::BtreeTrans<'_>, paths: &mut Paths<'_>,
                              bailed: bool) {
        let fs = self.fs();
        let mut o = self.owned.borrow_mut();
        let o = &mut *o;
        let unused: ArrayVec<_, { super::UPDATE_NODES_MAX * 2 }> =
            o.reserve.iter_mut().flat_map(|r| r.drain(..)).collect();

        for (b, free) in unused.iter().map(|b| (b, true)).chain(o.consumed.iter().map(|b| (b, bailed))) {
            let b = unsafe { b.as_ref() };

            if free {
                assert!(b.written.get() == 0, "a reserved node that's been written");

                let ob = unsafe { &mut (*ptr::from_ref(b).cast_mut()).ob };
                let mut cache = fs.btree.reserve_cache.lock.lock();
                if (cache.nr as usize) < cache.data.len() {
                    let nr = cache.nr as usize;
                    cache.nr += 1;
                    let a = &mut cache.data[nr];
                    a.ob = core::mem::take(ob);
                    a.k.set_key(unsafe { b.contents() }.key_i());
                } else {
                    ob.drain(fs).for_each(drop);
                }
                drop(cache);

                b.flags.set_need_write(false);
                node_free(fs, b);
                transition_state(fs, b, c::btree_node_cache_state::BTREE_NODE_CACHE_FREEABLE);
            }

            trans.node_unlock_write_pathless(paths, b);
            unsafe { six_unlock_ip(b.c.lock.six(), c::six_lock_type::SIX_LOCK_intent, crate::ret_ip!()) };
        }

        o.consumed.clear();
    }
}
