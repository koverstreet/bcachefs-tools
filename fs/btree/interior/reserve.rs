// SPDX-License-Identifier: GPL-2.0

//! An interior update's nodes: its reserve, allocated before it changes
//! anything - the worst case, splitting up to a new root - and given back when
//! it's done, the space of those it didn't use to the reserve cache; the nodes
//! it takes from it, set up; and the nodes it frees, freed in memory.

use core::mem::size_of;
use core::ptr;
use crate::alloc::capacity::DiskReservation;
use crate::alloc::foreground::{AllocWait, alloc_request, OpenBuckets};
use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{BkeySC, BkeyV, BkeyVMut};
use crate::btree::iter::CommitOpts;
use crate::btree::node::BtreeNode;
use crate::btree::node::sort::{btree_sort_into, build_aux_trees};
use crate::btree::path::{LockedNode, PathIdx, Paths};
use crate::c;
use crate::alloc::buckets::ReservationFlags;
use crate::alloc::write_point::WritePointSpecifier;
use crate::btree::cache::BtreeNodeCacheState;
use crate::btree::update::TransCommitFlags;
use crate::data::extents_format::BtreePtrV2;
use crate::data::write::WriteFlags;
use crate::init::dev::DevsList;
use crate::data::durability::BkeyVDurability;
use crate::data::ptrs::{BkeyVPtrs, BtreePtrs};
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::{POS_MIN, SPOS_MAX, prt};
use crate::format::{BkeyType, Bpos, DataType, BkeyFormat, BTREE_MAX_DEPTH};
use crate::fs::Fs;
use crate::os::kernel::WRITE;
use crate::util::{rcu::read_lock, sleep::Sleep};
use super::{BtreeUpdate, InteriorUpdate, NewNode, Reserved, Taken, TakenNode, BTREE_RESERVE_MAX};

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
fn format_fits(b: LockedNode<'_, BtreeNode>, new_f: &BkeyFormat) -> bool {
    let contents = b.contents();
    b.fits_u64s(contents.nr.u64s_with_format(&contents.format, new_f))
}

/* Btree node freeing/allocation: */

fn node_free(fs: &Fs, b: &BtreeNode) {
    fs.event_inc_trace(events::btree_node_free, |out| prt!(out, "{}", b.pos_to_text(fs)));

    assert!(!b.flags.write_blocked() && !b.flags.prev_write_blocked(), "freeing a write blocked node");
    assert!(!b.flags.need_write(), "freeing a node that needs writing");
    assert!(!fs.btree_id_root(b.c.btree_id.get().into()).is_some_and(|r| ptr::eq(r, b)), "freeing a root");

    b.flags.set_permanent(false);
    b.flags.set_noevict(false);
}

/// A new node's range: its key's, and its data's.
pub(super) fn set_range(b: &BtreeNode, contents: &mut crate::btree::node::NodeContents, min: Bpos, max: Bpos) {
    b.update_key(contents, |k| {
        if let Some(mut p) = (&mut **k).as_s().arm::<BtreePtrV2>() {
            p.v_mut().min_key = min;
        }
        k.k.p = max;
    });

    let data = contents.data_mut();
    data.min_key = min;
    data.max_key = max;
}

/// Not inlined: its device iterators aren't needed in the frame a node's
/// allocation runs under.
#[inline(never)]
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
    let available: u32 = devs.iter()
        .filter_map(|i| fs.dev_rcu(i as u32, &rcu))
        .map(|ca| ca.rw_durability())
        .sum();

    durability >= available
}

impl<'t> InteriorUpdate<'t> {
    /// The disk space, and the nodes, leaf and interior, that @nr_nodes says
    /// the update might need, reserved.
    pub(super) fn reserve(&self, paths: &mut Paths<'_>, nr_nodes: [usize; 2], target: u32, commit: CommitOpts,
                          write: WriteFlags) -> Result<(), BchError> {
        use crate::alloc::foreground::Watermark as wm;

        let t = self.t();
        let trans = t.trans();
        let fs = self.fs();
        let watermark = commit.get_watermark();
        let flags = commit.to_c();
        let disk_res_flags = if flags.contains(TransCommitFlags::no_enospc) {
            ReservationFlags::nofail
        } else {
            ReservationFlags::empty()
        };

        self.disk_res.add((nr_nodes[0] + nr_nodes[1]) as u64 * u64::from(fs.btree_sectors()),
                          u32::from(fs.opts.metadata_replicas.get()), disk_res_flags)?;

        let target = [target, fs.opts.metadata_target.get().into(), fs.opts.foreground_target.get().into()]
            .into_iter().find(|&t| t != 0).unwrap_or(0);
        let devs_have = DevsList::default();
        let nr_replicas = self.disk_res.nr_replicas.get();
        let mut req = paths.lend(|| t.alloc_request(target, false, Some(&devs_have), nr_replicas, nr_replicas,
                                                    watermark, write, AllocWait::Never))?;

        let ret = match self.reserve_get(paths, nr_nodes, &mut req, false) {
            Err(e) if e.matches(c::EAGAIN) || e.matches(c::ENOMEM) => {
                /*
                 * XXX: this should probably be a separate BTREE_INSERT_NONBLOCK
                 * flag
                 */
                if e.matches(c::EAGAIN) &&
                   flags.contains(TransCommitFlags::journal_reclaim) &&
                   (watermark as u32) < wm::reclaim as u32 {
                    return fs.throw(Errcode::journal_reclaim_would_deadlock);
                }

                req.wait = AllocWait::Sync(None);
                let mut low_mem = e.matches(Errcode::ENOMEM_btree_node_mem_alloc);

                loop {
                    let seen = fs.btree.cache.alloc_wait.snapshot();
                    match self.reserve_get(paths, nr_nodes, &mut req, low_mem) {
                        // low on memory: cannibalize - again, if a sleep in the allocator dropped the lock
                        Err(e) if e.matches(Errcode::ENOMEM_btree_node_mem_alloc) => low_mem = true,
                        Err(e) if e.matches(Errcode::btree_cache_cannibalize_lock_blocked) => {
                            trans.unlock_from(paths, crate::ret_ip!());
                            let Ok(()) = fs.btree.cache.alloc_wait.wait(seen, Sleep::UNINTERRUPTIBLE);
                        }
                        Err(e) if e.matches(Errcode::operation_blocked) => {
                            if let Err(e) = paths.lend(|| t.wait_on_allocator(&mut req, e)) {
                                break Err(e);
                            }
                        }
                        r => break r,
                    }
                }
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

    /// With @low_mem, under the cannibalize lock - but for a sleep in the
    /// allocator, which drops it: the nodes come from other cached nodes when
    /// memory can't be had, and the update with the lock gets all of them -
    /// two reserving at once, each with part, could deadlock.
    fn reserve_get(&self, paths: &mut Paths<'_>, nr_nodes: [usize; 2], req: &mut alloc_request<'_, 't>,
                   low_mem: bool) -> Result<(), BchError>
    {
        let trans = self.trans();

        assert!(nr_nodes[0] + nr_nodes[1] <= BTREE_RESERVE_MAX as usize,
                "an interior update reserving {} nodes", nr_nodes[0] + nr_nodes[1]);

        if low_mem {
            trans.btree_cache_cannibalize_trylock(req.may_wait())?;
        }

        let ret = (|| {
            for (interior, &nr) in nr_nodes.iter().enumerate() {
                while self.owned.borrow().reserve[interior].len() < nr {
                    let b = self.node_alloc_from_allocator(paths, interior != 0, req)?;
                    self.owned.borrow_mut().reserve[interior].push(b);
                }
            }
            Ok(())
        })();

        trans.btree_cache_cannibalize_unlock();
        ret
    }

    fn node_alloc_from_allocator(&self, paths: &mut Paths<'_>, interior: bool, req: &mut alloc_request<'_, 't>)
        -> Result<Reserved, BchError>
    {
        let t = self.t();
        let trans = t.trans();
        let fs = self.fs();

        let mut b = trans.btree_node_mem_alloc(paths, interior)?;
        let mut ob = OpenBuckets::default();

        let ret: Result<(), BchError> = (|| {
            let mut wp = loop {
                let wp = paths.lend(|| t.alloc_sectors(req, WritePointSpecifier::BTREE))?;
                if wp.sectors_free() >= fs.btree_sectors() {
                    break wp;
                }
                wp.retire_short(fs.btree_sectors());
            };

            let mut n = b.locked();

            let mut cache = fs.btree.reserve_cache.lock.lock();
            while cache.nr != 0 {
                cache.nr -= 1;
                let nr = cache.nr as usize;
                let a = &mut cache.data[nr];

                /* check if it has sufficient durability */

                let target = if req.flags.contains(WriteFlags::only_specified_devs) {
                    req.target
                } else {
                    0
                };
                if can_use_btree_node(fs, &self.disk_res, target, BkeySC::from(&*a.k)) {
                    n.set_key(&*a.k);
                    ob = core::mem::take(&mut a.ob);
                    return Ok(());
                }

                a.ob.drain(fs).for_each(drop);
            }
            drop(cache);

            n.update_key(|k| {
                let k = k.init();
                k.k.set_key_type(BkeyType::btree_ptr_v2);
                k.set_val_u64s(size_of::<BtreePtrV2>() / size_of::<u64>())
                    .expect("a btree node's key with room for a btree_ptr_v2");
                wp.append_ptrs(k, fs.btree_sectors(), false);
            });

            wp.open_buckets_get(&mut ob);
            Ok(())
        })();

        /*
         * Leave both intent and write held: nodes on the prealloc list stay
         * locked (they aren't reachable to any other trans — not yet hashed
         * in, or freshly hash-removed — so holding write blocks nothing).
         * Consumers can use them without any lock-take at pop time.
         */
        if let Err(e) = ret {
            b.to_freelist(fs);
            return Err(e);
        }
        Ok(Reserved { node: b, ob })
    }

    pub(crate) fn alloc_node(&self, level: usize) -> NewNode<'_> {
        let fs = self.fs();

        assert!(level < BTREE_MAX_DEPTH as usize, "allocating a node at level {level}");

        let Reserved { mut node, ob } = self.owned.borrow_mut().reserve[usize::from(level != 0)].pop()
            .expect("an interior update's reserve, used up");

        /*
         * The NewNode keeps owning intent+write refs on @b until it's
         * dropped.  Consumers (this function's callers) additionally take
         * their own intent+write recurses through the path machinery at the
         * WRITE_LOCKED mark site.
         */

        node.set_id(self.btree, level);

        let mut locked = node.locked();
        let n = locked.node();
        n.flags.set_accessed(true);

        let contents = locked.contents_mut();
        n.keys_mut(contents).bset_init_first();

        contents.nr = Default::default();
        let data = contents.data_mut();
        data.magic = crate::types::Le64::new(fs.bset_magic());
        data._ptr = Default::default();
        data.flags = Default::default();
        data.set_btree_id(self.btree);
        data.set_level(level as u8);
        let seq = data.keys.seq;

        n.update_key(contents, |k| if let Some(mut p) = (&mut **k).as_s().arm::<BtreePtrV2>() {
            let v = p.v_mut();
            v.mem_ptr.set(0);
            v.seq             = seq;
            v.sectors_written = Default::default();
        });

        contents.data_mut().set_new_extent_overwrite(true);

        build_aux_trees(n, contents);

        let state = n.live_state();
        let node = node.hash(fs, state).unwrap_or_else(|e| panic!("a new node's transition to live: {e}"));

        fs.event_inc_trace(events::btree_node_alloc, |out| prt!(out, "{}", node.pos_to_text(fs)));
        fs.io_clock[WRITE].increment(fs.btree_sectors().into());
        NewNode::new(self, Reserved { node, ob })
    }

    /// A copy of @b, compacted.
    pub(crate) fn alloc_replacement(&self, b: LockedNode<'_, BtreeNode>) -> NewNode<'_> {
        let fs = self.fs();
        let mut n = self.alloc_node(b.c.level.get().into());
        let b_contents = b.contents();
        let mut format = b.calc_format(b_contents);

        /*
         * The keys might expand with the new format - if they wouldn't fit in
         * the btree node anymore, use the old format for now:
         */
        if !format_fits(b, &format) {
            format = b_contents.format;
        }

        let mut v = n.locked();
        let (node, contents) = (v.node(), v.contents_mut());
        // a 32 bit counter, which wraps
        contents.data_mut().set_seq(b_contents.data().seq().wrapping_add(1));

        set_range(node, contents, b_contents.data().min_key, b_contents.data().max_key);

        contents.data_mut().format = format;
        contents.set_format(format);

        btree_sort_into(fs, &mut v, b);

        v.reset_sib_u64s(v.contents());
        n
    }

    /// A new root at @level, with no siblings to merge with: install_root()
    /// gives it to the path it's the root of.
    pub(crate) fn alloc_root(&self, level: usize) -> NewNode<'_> {
        let mut n = self.alloc_node(level);
        let mut v = n.locked();

        let (node, contents) = (v.node(), v.contents_mut());
        set_range(node, contents, POS_MIN, SPOS_MAX);
        let format = v.calc_format(v.contents());

        let contents = v.contents_mut();
        contents.data_mut().format = format;
        contents.set_format(format);
        build_aux_trees(v.node(), v.contents_mut());

        for sib in &v.contents().sib_u64s {
            sib.set(u16::MAX);
        }
        n
    }

    /// New root @n, given to @path above what it held.
    pub(crate) fn install_root(&self, paths: &mut Paths<'_>, path: PathIdx, n: NewNode<'_>) -> TakenNode {
        let level = usize::from(n.c.level.get());
        let locks_want = paths[path].locks_want();
        paths[path].bits.set_locks_want(locks_want + 1);
        assert!(!paths[path].locked(level), "a new root's level already locked");
        self.take_node(paths, path, n)
    }

    /// @n, given to a path of its own: the update puts the path.
    pub(crate) fn take_path(&self, paths: &mut Paths<'_>, n: NewNode<'_>) -> TakenNode {
        let pos = n.held().key().k().p;
        let idx = self.trans().path_get_unlocked_mut(paths, crate::format::Btree(n.c.btree_id.get().into()),
                                                     n.c.level.get().into(), pos, false, crate::ret_ip!());
        self.owned.borrow_mut().paths.push(idx);
        self.take_node(paths, idx, n)
    }

    /*
     * Install a freshly-consumed prealloc node on @path at WRITE_LOCKED: the
     * NewNode's own intent+write, from bch2_btree_node_mem_alloc, become the
     * path's - its only hold - and the node is reached through it from now.
     *
     * When the path unlocks - put by the update, or for a root, its write
     * dropped by done() - btree_node_unlock for WRITE_LOCKED drops that intent
     * and write, its write unlock advancing the paths at it. Never written,
     * it's taken back from the path and freed.
     */
    fn take_node(&self, paths: &mut Paths<'_>, path: PathIdx, n: NewNode<'_>) -> TakenNode {
        let fs = self.fs();
        let level = usize::from(n.c.level.get());
        let Reserved { node, ob } = n.into_reserved();
        let (b, held) = node.into_held(fs);

        self.trans().path_level_init(paths, path, level, LockedNode::of_hold(b, &held));
        paths[path].put_level(level, &b.c, held);
        paths[path].lock_taken(level);

        let at = TakenNode { path, level };
        self.owned.borrow_mut().taken.push(Taken { at, ob });
        at
    }

    /// @b, freed by the update, freed in memory: @path holds it write locked.
    pub(super) fn node_free_inmem(&self, paths: &mut Paths<'_>, path: PathIdx, level: usize) {
        let fs = self.fs();

        assert!(paths[path].write_locked(level), "freeing a node not write locked");

        let b = paths[path].locked_btree_node(level).expect("a node to free, held by its path");
        node_free(fs, &b);

        let _ = fs.btree_node_transition_state(&b, BtreeNodeCacheState::FREEABLE);
        let b = ptr::from_ref(b.node());
        self.trans().node_forget(paths, b);
    }
}

impl BtreeUpdate {
    /// The nodes of the update's reserve it didn't take, freed.
    pub(super) fn reserve_put(&self) {
        for r in self.owned.borrow_mut().reserve.iter_mut().flat_map(|r| r.drain(..)) {
            self.reserve_free(r);
        }
    }

    /// A node from the reserve, never written, freed: its space kept in the
    /// reserve cache if there's room.
    pub(super) fn reserve_free(&self, Reserved { node, mut ob }: Reserved) {
        let fs = self.fs();
        assert!(node.written.get() == 0, "a reserved node that's been written");

        let mut cache = fs.btree.reserve_cache.lock.lock();
        if (cache.nr as usize) < cache.data.len() {
            let nr = cache.nr as usize;
            cache.nr += 1;
            let a = &mut cache.data[nr];
            a.ob = core::mem::take(&mut ob);
            a.k.set_key(&*node.key_peek());
        } else {
            ob.drain(fs).for_each(drop);
        }
        drop(cache);

        node.flags.set_need_write(false);
        node_free(fs, &node);
        node.to_freelist(fs);
    }
}
