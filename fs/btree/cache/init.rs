// SPDX-License-Identifier: GPL-2.0

/* Filesystem init / exit */

use super::{cached_node, new_node_lock, node_mem_alloc, node_mem_free};
use crate::btree::node::BtreeNode;
use crate::btree::types::BtreeCacheGuard;
use crate::c;
use crate::data_structures::list::Walk;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::locking::six::six_lock_counts;
use crate::prt;
use crate::util::kernel::cond_resched;
use crate::util::log::LogMsg;
use crate::util::place::PlacedDrop;
use c::BtreeNodeCacheState as State;
use c::SixLockType::{SIX_LOCK_intent, SIX_LOCK_read, SIX_LOCK_write};
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;

/*
 * Diagnostic for the cache_exit teardown trylocks: if the trylock fails
 * something else is still holding intent or write on a node we expect to
 * be quiesced. Dump everything that might identify the holder before we
 * BUG, since there's no other forensic output for this case.
 */
#[cold]
#[inline(never)]
fn btree_cache_exit_locked_dump(fs: &Fs, lists: &BtreeCacheGuard<'_>, b: &BtreeNode, what: &str) {
    let lock = b.c.lock.six();
    let counts = six_lock_counts(lock);
    let state = b.cache_state(lists);

    let mut msg = LogMsg::new(fs);
    prt!(msg, "{what} on {} node at cache_exit\n",
         if state == State::FREEABLE { "freeable" } else { "live" });
    prt!(msg, "btree={} level={} cache_state={} flags=0x{:x} hash_val=0x{:x}\n",
         b.c.btree_id, b.c.level, state as u32, b.flags.load().into_bits(), b.hash_val());
    prt!(msg, "lock state=0x{:x} read={} intent={} write={} (recurse intent={} write={})\n",
         lock.state.load(Relaxed),
         counts.n[SIX_LOCK_read as usize],
         counts.n[SIX_LOCK_intent as usize],
         counts.n[SIX_LOCK_write as usize],
         lock.intent_lock_recurse.get(),
         lock.write_lock_recurse.get());

    let owner = lock.owner.load(Relaxed);
    if owner.is_null() {
        prt!(msg, "Owner: (not recorded)\n");
    } else {
        let comm = unsafe { core::ffi::CStr::from_ptr((*owner).comm.as_ptr().cast()) };
        prt!(msg, "Owner: {}\n", comm.to_str().unwrap_or("?"));
    }
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    {
        prt!(msg, "Last taken:\n");
        crate::util::backtrace::prt_backtrace(&mut msg, unsafe { &*lock.owner_stack.get() });
    }
    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    if !owner.is_null() {
        let _ = crate::util::backtrace::prt_task_backtrace(&mut msg, owner, 0);
    }
}

impl Fs {
    pub fn btree_cache_init(&self) -> Result<(), BchError> {
        let bc = &self.btree.cache;

        self.recalc_btree_reserve();

        for _ in 0..bc.lock.nr_reserve.get() {
            let Some(b) = node_mem_alloc(self) else {
                return self.throw(Errcode::BCH_ERR_ENOMEM_fs_btree_cache_init);
            };
            let b = unsafe { cached_node(self, b.as_ptr()) };
            let held = new_node_lock(b);

            let _ = self.btree_node_transition_state(b, State::FREEABLE);

            drop(held);
        }

        bc.shrinkers_register(self)
    }

    fn btree_cache_exit_drain_node(&self, lists: &mut BtreeCacheGuard<'_>, b: &BtreeNode) {
        assert!(!(b.flags.read_in_flight() || b.flags.write_in_flight()), "IO in flight on a node at cache exit");
        let Some(intent) = b.c.lock.try_lock_as(SIX_LOCK_intent) else {
            btree_cache_exit_locked_dump(self, lists, b, "intent held");
            panic!("intent held on a node at cache exit");
        };
        let Ok(held) = intent.try_write() else {
            btree_cache_exit_locked_dump(self, lists, b, "write held");
            panic!("write held on a node at cache exit");
        };

        /*
         * Dirty nodes can survive to exit on a dead journal: the going-RO
         * path's bch2_btree_cancel_all_writes() only catches nodes dirty at
         * that instant, and the window between going-RO and unmount can
         * redirty a node (btree node reads wanting a rewrite, interior
         * update teardown). With the journal dead they can never be written;
         * cancel here too. Dirty at exit with a live journal is a bug, but
         * we're freeing everything regardless — warn, don't die.
         */
        if b.flags.dirty() {
            crate::warn_on_once!(self.journal.error().is_ok());
            b.flags.set_dirty(false);
        }

        let _ = self.btree_node_transition_state_locked(lists, b, State::FREED);
        drop(held);
        cond_resched();
    }

    /// # Safety
    /// Nothing uses the cache after.
    pub unsafe fn btree_cache_exit(&self) {
        let bc = &self.btree.cache;

        for s in bc.shrinkers.iter().rev() {
            unsafe { s.placed_drop() }
        }

        /*
         * btree_node_write_work clears BTREE_NODE_write_in_flight before
         * releasing its read lock on the node, so flush_all_writes returning
         * doesn't guarantee the lock is unheld. Drain the workqueue so the
         * walks below can rely on trylock succeeding without racing with
         * still-finishing post-write work.
         */
        if self.btree.write_complete_wq.is_made() {
            self.btree.write_complete_wq.drain();
        }

        /* vfree() can allocate memory: */
        {
            let mut lists = bc.lock.lock();

            for i in 0..lists.live.len() {
                for dirty in [true, false] {
                    let mut walk = unsafe {
                        Walk::new(&mut lists, |l| if dirty { &mut l.live[i].dirty } else { &mut l.live[i].clean })
                    };
                    while let Some((lists, b)) = walk.next() {
                        b.flags.set_permanent(false);
                        self.btree_cache_exit_drain_node(lists, b);
                    }
                }
            }

            let mut walk = unsafe { Walk::new(&mut lists, |l| &mut l.freeable) };
            while let Some((lists, b)) = walk.next() {
                self.btree_cache_exit_drain_node(lists, b);
            }

            assert!(self.journal.error().is_err() ||
                    (bc.lock.live_nr[0].nr_dirty.get() == 0 && bc.lock.live_nr[1].nr_dirty.get() == 0),
                    "dirty nodes at cache exit, with the journal live");

            let l = &mut *lists;
            for freed in [&mut l.freed_pcpu, &mut l.freed_nonpcpu] {
                let mut cur = freed.cursor_mut();
                while let Some(b) = cur.remove_current() {
                    unsafe { node_mem_free(self, NonNull::from(b)) };
                }
            }

            for (i, &nr) in lists.nr_by_btree.iter().enumerate() {
                assert!(nr == 0, "{nr} nodes of btree {i} at cache exit");
            }
            let live = &bc.lock.live_nr;
            crate::warn_on!(live[0].nr_clean.get() != 0);
            crate::warn_on!(live[0].nr_dirty.get() != 0);
            crate::warn_on!(live[1].nr_clean.get() != 0);
            crate::warn_on!(live[1].nr_dirty.get() != 0);
            crate::warn_on!(bc.lock.nr_freeable.get() != 0);
            crate::warn_on!(bc.nr_in_flight.load(Relaxed) != 0);
            crate::warn_on!(bc.nr_in_flight_inner.load(Relaxed) != 0);
        }

        drop(core::mem::take(&mut bc.roots.lock().roots_extra));
    }
}
