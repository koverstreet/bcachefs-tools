// SPDX-License-Identifier: GPL-2.0

use super::reclaim::ReclaimFlags;
use super::{cache_node_struct_alloc, cached_node, NodeBufs, BtreeCacheGuard, BtreeNodeList};
use crate::btree::bkey::{bkey_val_to_text, BkeyI, BkeySC};
use crate::util::{alloc::{flags, Flags}, time_stats::TimeStat};
use crate::btree::iter::BtreeTrans;
use crate::btree::node::{BtreeNode, BtreeWriteType};
use crate::btree::path::{btree_path_to_text, LockedNode, PathIdx, Paths, Write};
use crate::btree::read::{btree_node_wait_on_read, btree_node_wait_on_write, node_read, ReadInFlightNode};
use crate::btree::write::{node_write, BtreeWriteFlags};
use crate::c;
use crate::data::extents_format::BKEY_BTREE_PTR_U64s_MAX;
use crate::data_structures::list::Walk;
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::format::{Btree, BTREE_MAX_DEPTH};
use crate::fs::Fs;
use crate::locking::six::SixHold;
use crate::prt;
use crate::os::{kernel::{cond_resched, local_clock, unlikely}, sched::current};
use crate::btree::cache::BtreeNodeCacheState as State;
use crate::locking::six::SixLockType::{self, SIX_LOCK_intent, SIX_LOCK_read, SIX_LOCK_write};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering::Relaxed;

/* Btree node alloc / lookup / evict */

impl Fs {
    /// The first node on @list with the read locks wanted that reclaim gives
    /// us, write locked - still in the cache.
    fn btree_node_reclaim_first(&self, lists: &mut BtreeCacheGuard<'_>,
                                list: impl for<'l, 'g> Fn(&'l mut BtreeCacheGuard<'g>) -> &'l mut BtreeNodeList,
                                pcpu_read_locks: bool, flags: ReclaimFlags) -> Option<(&BtreeNode, SixHold<'_>)> {
        let mut walk = unsafe { Walk::new(lists, list) };
        while let Some((lists, b)) = walk.next() {
            let b = unsafe { cached_node(self, b) };
            if pcpu_read_locks != b.c.lock.six().readers.allocated() {
                continue;
            }
            if let Ok(held) = self.btree_node_reclaim(lists, b, flags) {
                return Some((b, held));
            }
        }

        None
    }

    fn btree_node_grab(&self, list: impl for<'l, 'g> Fn(&'l mut BtreeCacheGuard<'g>) -> &'l mut BtreeNodeList,
                       pcpu_read_locks: bool) -> Option<OwnedNode> {
        let mut lists = self.btree.cache.lock.lock();
        let (b, held) = self.btree_node_reclaim_first(&mut lists, list, pcpu_read_locks, ReclaimFlags::empty())?;

        self.btree.evicted_size.record(b.hash_val(), LockedNode::of_hold(b, &held).contents().nr.live_u64s);
        Some(OwnedNode::take(self, &mut lists, b, held))
    }

    fn system_memory_usage_high(&self) -> bool {
        let avail = crate::os::kernel::mem_available_bytes();
        let total = crate::os::kernel::totalram_bytes();
        if avail >= total >> 2 {
            return false;
        }

        let bc = &self.btree.cache;
        let pinned = (bc.lock.nr_live() + bc.lock.nr_freeable.get()) * self.opts.btree_node_size.get() as usize;
        pinned > (total - avail) >> 2
    }
}

/// A node the allocator handed out, its lock held as @M - for Write, the write
/// lock and the intent under it - and its owner's alone: in neither the table
/// nor a list, NONE, no one else can find it until hash() puts it in the cache,
/// still the owner's. Owned outside any borrow - an interior update's reserve
/// has its nodes across calls - it's put back before the cache exits, by
/// unlock(), to_freelist(), free() or into_held(): dropped, it panics.
pub(crate) struct OwnedNode<M = Write> {
    b:     NonNull<BtreeNode>,
    _mode: PhantomData<M>,
}

impl Deref for OwnedNode<Write> {
    type Target = BtreeNode;

    fn deref(&self) -> &BtreeNode {
        unsafe { self.b.as_ref() }
    }
}

impl OwnedNode<Write> {
    fn new(b: &BtreeNode, held: SixHold<'_>) -> Self {
        assert!(!BtreeNodeList::linked(b), "an owned node on a list");
        assert!(b.hash_val() == 0, "an owned node hashed");
        Self::from_path(b, held)
    }

    /// A new struct of the cache's, with @gfp: no one else has it to hold its
    /// locks.
    pub(super) fn alloc(fs: &Fs, pcpu_read_locks: bool, gfp: Flags) -> Option<Self> {
        let b = cache_node_struct_alloc(fs, pcpu_read_locks, gfp)?;
        let held = b.c.lock.try_lock_as(SIX_LOCK_intent)
            .and_then(|intent| intent.try_write().ok())
            .expect("a new node locked by someone else");
        Some(Self::new(b, held))
    }

    /// @b, write locked by @held, taken out of the cache: unhashed, off its
    /// list.
    pub(super) fn take(fs: &Fs, lists: &mut BtreeCacheGuard<'_>, b: &BtreeNode, held: SixHold<'_>) -> Self {
        let _ = fs.btree_node_transition_state_locked(lists, b, State::NONE);
        Self::new(b, held)
    }

    /// @b, an owner's that a path held since, back from it by @held - its
    /// hold, write: in the cache still, if hash() put it there.
    pub(crate) fn from_path(b: &BtreeNode, held: SixHold<'_>) -> Self {
        assert!(held.ty() == SIX_LOCK_write && ptr::eq(held.lock(), &b.c.lock),
                "an owned node without its write lock");
        held.into_raw();
        OwnedNode { b: NonNull::from(b), _mode: PhantomData }
    }

    pub(crate) fn locked(&mut self) -> LockedNode<'_, BtreeNode, Write> {
        LockedNode::of_owned(self)
    }

    pub(crate) fn held(&self) -> LockedNode<'_, BtreeNode> {
        LockedNode::of_owned_held(self)
    }

    pub(crate) fn set_id(&mut self, btree: Btree, level: usize) {
        self.c.level.set(level as u8);
        self.c.btree_id.set(btree.0 as u8);
    }

    /// Into the cache as @state, CLEAN or DIRTY, and found by lookups from
    /// then - still the owner's, locked. Failing, it's put back.
    pub(crate) fn hash(self, fs: &Fs, state: State) -> Result<Self, BchError> {
        match fs.btree_node_transition_state(&self, state) {
            Ok(()) => Ok(self),
            Err(e) => {
                self.to_freelist(fs);
                Err(e)
            }
        }
    }

    /// Given to a holder that borrows the filesystem, as the node and its
    /// hold: a cache node lives as long as its filesystem.
    pub(crate) fn into_held<'f>(self, fs: &'f Fs) -> (&'f BtreeNode, SixHold<'f>) {
        let this = ManuallyDrop::new(self);
        unsafe {
            let b = cached_node(fs, this.b.as_ptr());
            (b, SixHold::from_raw(&b.c.lock, SIX_LOCK_write))
        }
    }

    fn hold(&self) -> ManuallyDrop<SixHold<'_>> {
        ManuallyDrop::new(unsafe { SixHold::from_raw(&self.c.lock, SIX_LOCK_write) })
    }

    /// Its locks released, no path's lock seq moved: for a node no path has.
    pub(crate) fn unlock(self) {
        let this = ManuallyDrop::new(self);
        drop(ManuallyDrop::into_inner(this.hold()));
    }

    /// Put back FREEABLE, with its buffers, its locks released.
    pub(crate) fn to_freelist(self, fs: &Fs) {
        let _ = fs.btree_node_transition_state(&self, State::FREEABLE);
        self.unlock();
    }

    /// Its buffers freed, and put back FREED - a struct for the next
    /// allocation - its locks released.
    pub(super) fn free(mut self, fs: &Fs, lists: &mut BtreeCacheGuard<'_>) {
        ebug_assert!(!self.flags.write_in_flight(), "freeing the buffers of a node with a write in flight");
        self.flags.set_just_written(false);
        self.locked().contents_mut().bufs.free_accounted();
        let _ = fs.btree_node_transition_state_locked(lists, &self, State::FREED);
        self.unlock();
    }
}

impl<M> Drop for OwnedNode<M> {
    fn drop(&mut self) {
        panic!("an owned btree node dropped, not put back");
    }
}

impl BtreeTrans<'_> {
    fn btree_node_cannibalize(&self, paths: &mut Paths<'_>, pcpu_read_locks: bool) -> OwnedNode {
        let fs = self.fs();

        loop {
            for i in 0..2 {
                if let Some(got) = fs.btree_node_grab(|l| &mut l.live[i].clean, pcpu_read_locks) {
                    return got;
                }
            }

            /*
             * None clean: write a dirty one, and take it clean on the next
             * pass. Written while it's still in the cache - taking it out
             * clears its dirty bit, and the write never happens - and not
             * under bc->lock, or waited on with it write locked: the write's
             * completion takes both.
             */
            let dirty = (0..2).find_map(|i| {
                let mut lists = fs.btree.cache.lock.lock();
                fs.btree_node_reclaim_first(&mut lists, |l| &mut l.live[i].dirty, pcpu_read_locks,
                                            ReclaimFlags::ALLOW_DIRTY)
            });
            if let Some((b, held)) = dirty {
                if b.flags.dirty() {
                    node_write(self.fs(), LockedNode::of_hold(b, &held), BtreeWriteType::cache_reclaim,
                               BtreeWriteFlags::empty());
                }
                drop(held);

                btree_node_wait_on_read(self, paths, b);
                btree_node_wait_on_write(self, paths, b);
                continue;
            }

            /*
             * Rare case: all matching-type nodes were intent-locked.
             * Just busy-wait.
             */
            crate::warn_once!(true, fs, "btree cache cannibalize failed");
            cond_resched();
        }
    }

    pub(crate) fn btree_node_mem_alloc(&self, paths: &mut Paths<'_>, pcpu_read_locks: bool)
        -> Result<OwnedNode, BchError>
    {
        let fs = self.fs();
        let bc = &fs.btree.cache;
        let start_time = local_clock();

        let (mut n, new_bufs) = 'got_mem: {
            if let Some(n) = fs.btree_node_grab(|l| &mut l.freeable, pcpu_read_locks) {
                break 'got_mem (n, None);
            }

            /*
             * If MM is tight AND we're a meaningful fraction of in-use memory,
             * reclaim from our own cache instead of asking MM. MM's shrinker
             * callbacks haven't been aggressive enough to keep bcachefs out of
             * OOM under heavy load (TiCPU report).
             */
            if unlikely(fs.system_memory_usage_high()) {
                bc.nr_self_reclaim.fetch_add(1, Relaxed);
                let live = pcpu_read_locks as usize;
                if let Some(n) = fs.btree_node_grab(|l| &mut l.live[live].clean, pcpu_read_locks) {
                    break 'got_mem (n, None);
                }
            }

            'err: {
                let mut bufs = NodeBufs::new(fs);
                let buf_start = local_clock();
                let got = bufs.alloc(fs, flags::NOWAIT, true).is_ok() || {
                    self.unlock_keep_cannibalize(paths, crate::ret_ip!());
                    bufs.alloc(fs, flags::NOWARN, true).is_ok()
                };
                fs.times.get(TimeStat::btree_node_buf_alloc as usize).update(buf_start);
                if !got {
                    bufs.free();
                    break 'err;
                }

                let freed = fs.btree_node_grab(|l| if pcpu_read_locks { &mut l.freed_pcpu } else { &mut l.freed_nonpcpu },
                                               pcpu_read_locks);
                let n = freed
                    .or_else(|| OwnedNode::alloc(fs, pcpu_read_locks, flags::NOWAIT))
                    .or_else(|| {
                        self.unlock_keep_cannibalize(paths, crate::ret_ip!());
                        OwnedNode::alloc(fs, pcpu_read_locks, Flags::empty())
                    });
                let Some(n) = n else {
                    bufs.free();
                    break 'err;
                };

                break 'got_mem (n, Some(bufs));
            }

            /* Try to cannibalize another cached btree node: */
            if bc.alloc_lock.load(Relaxed) == current() {
                let n = self.btree_node_cannibalize(paths, pcpu_read_locks);
                fs.event_inc_trace(events::btree_cache_cannibalize, |out| prt!(out, "{}", self.fn_name()));
                break 'got_mem (n, None);
            }

            fs.times.get(TimeStat::btree_node_mem_alloc as usize).update(start_time);
            return Err(Errcode::ENOMEM_btree_node_mem_alloc.into());
        };

        assert!(!n.flags.dirty(), "a dirty node allocated");
        assert!(!n.flags.write_in_flight(), "a node allocated with a write in flight");

        /* it may have been a node our paths still have cached: */
        self.node_forget(paths, &*n);

        let mut locked = n.locked();
        let b = locked.node();
        b.flags.store(Default::default());
        b.written.set(0);
        let contents = locked.contents_mut();
        if let Some(bufs) = new_bufs {
            contents.bufs = bufs;
        }
        assert!(!contents.bufs.data.is_null(), "a node allocated with no buffer");
        contents.nsets = 0;
        contents.sib_u64s[0].set(0);
        contents.sib_u64s[1].set(0);
        b.whiteout_u64s.set(0);
        b.keys_mut(contents).btree_keys_init();

        fs.times.get(TimeStat::btree_node_mem_alloc as usize).update(start_time);

        /*
         * Room in the table for the node's hash insert, which grows it under
         * bc->lock, where it can't block: made here, with our locks dropped
         * if it has to block.
         */
        let _ = self.allocate_dropping_locks_norelock(paths, || bc.table.make_room().ok());

        if let Err(e) = self.relock_from(paths) {
            n.to_freelist(fs);
            return Err(e);
        }
        Ok(n)
    }

    /* Slowpath, don't want it inlined into btree_iter_traverse() */
    #[inline(never)]
    pub(super) fn btree_node_fill(&self, paths: &mut Paths<'_>, path: Option<PathIdx>, k: &BkeyI, btree: Btree,
                                  level: usize, lock_type: SixLockType, sync: bool)
        -> Result<Option<(&BtreeNode, SixHold<'_>)>, BchError>
    {
        let fs = self.fs();

        ebug_assert!(path.is_none_or(|p| level + 1 == paths[p].level() as usize));

        if unlikely(level >= BTREE_MAX_DEPTH as usize) {
            let mut msg = fs.log().msg();
            prt!(msg, "attempting to get btree node at level {level}, >= max depth {}", BTREE_MAX_DEPTH);
            return Err(fs.topology_error(&mut msg));
        }

        if unlikely(!k.k.is_btree_ptr()) {
            let mut msg = fs.log().msg();
            prt!(msg, "attempting to get btree node with non-btree key ");
            bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(k));
            return Err(fs.topology_error(&mut msg));
        }

        if unlikely(usize::from(k.k.u64s) > BKEY_BTREE_PTR_U64s_MAX) {
            let mut msg = fs.log().msg();
            prt!(msg, "attempting to get btree node with too big key ");
            bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(k));
            return Err(fs.topology_error(&mut msg));
        }

        /*
         * Parent node must be locked, else we could read in a btree node that's
         * been freed:
         */
        if let Some(p) = path {
            self.path_relock(paths, p)?;
        }

        let mut n = match self.btree_node_mem_alloc(paths, level != 0) {
            Err(e) if e.matches(c::ENOMEM) => {
                let Some(p) = path else { return Err(e) };

                self.set_memory_allocation_failure(true);

                fs.event_inc_trace(events::trans_restart_memory_allocation_failure, |out| {
                    prt!(out, "{}\n", self.fn_name());
                    btree_path_to_text(out, self, p, &paths[p]);
                });
                return Err(self.restart(Errcode::transaction_restart_fill_mem_alloc_fail));
            }
            r => r?,
        };

        n.locked().set_key(k);
        n.set_id(btree, level);

        let ret = match n.hash(fs, State::CLEAN).map(|n| n.into_held(fs)) {
            Ok((b, held)) => {
                /*
                 * Must be set with node write locked - b->data is not stable
                 * while node is write locked or read_in_flight is set, read
                 * path will do a buffer swap
                 *
                 * and also cache coherency
                 */
                let read = unsafe { ReadInFlightNode::new(b) };
                let intent = held.downgrade();

                let held = if path.is_some() {
                    let seq = b.c.lock.six().seq.get();

                    /* Unlock before doing IO: */
                    drop(intent);
                    self.unlock_from(paths, crate::ret_ip!());

                    node_read(self, read, sync);

                    if sync { b.c.lock.relock_as(lock_type, seq) } else { None }
                } else {
                    node_read(self, read, sync);
                    Some(if lock_type == SIX_LOCK_read { intent.downgrade() } else { intent })
                };
                held.map(|held| (b, held))
            }
            /* raced with another fill: */
            Err(_) => None,
        };

        /*
         * bch2_btree_node_mem_alloc may have unlocked the trans for GFP_KERNEL
         * allocation, and the if (path) { } block above unlocks for IO - ensure
         * we're relocked before returning:
         */
        if let Some(p) = path {
            self.relock_from(paths).and_then(|()| self.path_relock(paths, p))?;
        }

        Ok(ret)
    }
}
