// SPDX-License-Identifier: GPL-2.0

//! Btree path traversal: walking a path down from its root to its level, the
//! nodes on the way locked, and back up to where it's still good.

use core::cmp::Ordering;
use core::ffi::{c_uint, c_ulong};
use core::ptr::{self, NonNull};


use crate::btree::bkey::{bkey_val_to_text, BkeyI, BkeySC, BkeyV, BKEY_U64S, POS_MIN, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::cache::bch2_btree_id_level_to_text;
use crate::btree::interior::check_topology_msg;
use crate::btree::iter::{BtreeIterFlags, BtreeTrans, TransAttempt};
use crate::btree::journal_overlay::BtreeAndJournalIter;
use crate::btree::node::iter::KeyIter;
use crate::btree::node::keys::Keys;
use crate::btree::node::BtreeNode;
use crate::btree::path::{BtreePath, DownKey, KeyVal, LockedNode, NodePtr, PathIdx, Paths};
use crate::c;
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::format::{BkeyType, Bpos};
use crate::fs::Fs;
use crate::prt;
use crate::util::backtrace::prt_task_backtrace;
use crate::util::closure::StackClosure;
use crate::util::kernel::race_fault;
use crate::util::log::LogMsg;
use crate::util::vstructs::{plain_bytes, plain_bytes_mut};
use pin_init::stack_pin_init;

// ── A level: its node, and its node iterator ────────────────────────────────

pub(crate) fn advance_to_pos(iter: &mut KeyIter, keys: &Keys<'_>, pos: &Bpos, max_advance: i32) -> bool {
    let mut nr_advanced = 0;

    while let Some(k) = iter.peek_all() {
        if keys.bkey_iter_pos_cmp(k, pos).is_ge() {
            break;
        }

        if max_advance > 0 && nr_advanced >= max_advance {
            return false;
        }

        iter.advance(keys);
        nr_advanced += 1;
    }

    true
}

fn level_unpack(keys: &Keys<'_>, k: Option<u16>, u: &mut c::bkey) -> Option<KeyVal> {
    match k {
        Some(k) => Some(KeyVal(keys.disassemble(k, u).v)),
        None => {
            /*
             * signal to bch2_btree_iter_peek_slot() that we're currently at
             * a hole
             */
            u.set_key_type(BkeyType::deleted);
            None
        }
    }
}

impl BtreePath {
    #[inline]
    pub(crate) fn level_init_iter(&mut self, fs: &Fs, level: usize, b: LockedNode<'_, BtreeNode>) {
        let keys = b.keys(b.contents());
        let iter = &mut self.l[level].iter;

        iter.init(fs, &keys, &self.pos);

        /*
         * Iterators to interior nodes should always be pointed at the first non
         * whiteout:
         */
        if level != 0 {
            iter.peek(&keys);
        }
    }

    pub(crate) fn level_peek_all(&self, u: &mut c::bkey) -> Option<KeyVal> {
        let level = self.level() as usize;
        let b = self.locked_btree_node(level).expect("a path peeked is locked");
        let keys = b.keys(b.contents());
        level_unpack(&keys, self.l[level].iter.peek_all(), u)
    }

    #[inline(always)]
    fn check_pos_in_node(&self, l: usize, check_pos: Ordering) -> bool {
        let b = || self.locked_btree_node(l).expect("a level checked is locked");
        match check_pos {
            Ordering::Less    => !self.pos_before_node(b()),
            Ordering::Greater => !self.pos_after_node(b()),
            Ordering::Equal   => true,
        }
    }
}

impl BtreeTrans<'_> {
    #[inline]
    pub(crate) fn path_level_init(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize,
                                  b: LockedNode<'_, BtreeNode>) {
        let path = &mut paths[idx];
        let unread = b.flags.read_error();
        ebug_assert!(!path.cached());
        ebug_assert!(unread || path.pos_in_node(b));

        path.l[level].lock_seq = b.c.lock.six().seq.get();
        /* deadlock detector reads unlocked */
        unsafe { ptr::write_volatile(&raw mut path.l[level].b, NodePtr::node(NonNull::from(&b.c))) };
        if unread {
            path.l[level].iter = KeyIter::default();
        } else {
            path.level_init_iter(self.fs(), level, b);
        }
    }

    pub(crate) fn path_level_prev(&self, paths: &mut Paths<'_>, idx: PathIdx, u: &mut c::bkey) -> Option<KeyVal> {
        let path = &mut paths[idx];
        let level = path.level() as usize;

        ebug_assert!(path.ref_ == 1);

        let (k, pos) = {
            let (b, iter) = path.locked_level_mut(level).expect("a path stepped back is locked");
            let keys = b.keys(b.contents());
            match iter.prev(&keys) {
                Some(k) => {
                    let k = keys.disassemble(k, u);
                    (Some(KeyVal(k.v)), k.k.p)
                }
                None => (level_unpack(&keys, None, u), b.contents().data().min_key),
            }
        };

        path.pos = pos;
        self.set_paths_sorted(false);
        k
    }

    pub(crate) fn path_traverse(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, idx: PathIdx,
                                flags: BtreeIterFlags) -> Result<(), BchError>
    {
        self.verify_not_unlocked_or_in_restart();

        if paths[idx].nodes_locked == 0 {
            self.path_traverse_one(t, paths, idx, flags)
        } else {
            Ok(())
        }
    }

    fn path_set_level_up(&self, paths: &mut Paths<'_>, idx: PathIdx, l: usize) {
        self.node_unlock(paths, idx, l);
        paths[idx].l[l].b = NodePtr::err(Errcode::BCH_ERR_no_btree_node_up.into());
    }
}

// ── Traversal ───────────────────────────────────────────────────────────────

fn root_unpack(packed: c_ulong) -> (Option<NonNull<BtreeNode>>, usize) {
    let mask = (1 << c::BTREE_ROOT_LEVEL_BITS) - 1;
    let level = (packed & mask) as usize;
    // journal_entry_btree_root_validate() rejects a root any deeper
    invariant!(level < c::BTREE_MAX_DEPTH as usize);
    (NonNull::new((packed & !mask) as *mut BtreeNode), level)
}

fn root_packed(fs: &Fs, btree: crate::format::Btree) -> c_ulong {
    fs.btree.cache.roots.roots_b.get(btree.0 as usize).map_or(0, |r| r.get())
}

fn bpos_in_btree_node_key(pos: Bpos, k: &BkeyI) -> bool {
    if pos > k.k.p {
        return false;
    }

    if BkeySC::from(k).arm::<c::BtreePtrV2>().is_some_and(|p| pos < p.v().min_key) {
        return false;
    }

    true
}

/// @v: the value @dst was copied from.
fn down_key_mem_ptr_reload(dst: &mut DownKey, v: *const u64) {
    if dst.k.key_type() == BkeyType::btree_ptr_v2 {
        // mem_ptr: the value's first field
        dst.u64s_mut()[BKEY_U64S] = unsafe { ptr::read_volatile(v) };
    }
}

fn set_down_key(dst: &mut DownKey, k: BkeySC<'_>) {
    let val = k.val_bytes();
    let room = plain_bytes_mut(dst.u64s_mut());

    room[..size_of::<c::bkey>()].copy_from_slice(plain_bytes(k.k));
    room[size_of::<c::bkey>()..][..val.len()].copy_from_slice(val);
    down_key_mem_ptr_reload(dst, val.as_ptr().cast());
}

fn set_down_key_packed(dst: &mut DownKey, keys: &Keys<'_>, k: u16) {
    let f = &keys.contents().format;
    let key = keys.key_at(k);

    key.unpacked_into(f, keys.unpack(k), dst.u64s_mut());
    down_key_mem_ptr_reload(dst, key.val(f).as_ptr());
}

impl BtreeTrans<'_> {
    #[cold]
    #[inline(never)]
    fn node_root_err(&self, b: &BtreeNode) -> BchError {
        let fs = self.fs();
        let mut msg = LogMsg::new(fs);

        prt!(&mut *msg, "btree root doesn't cover expected range:\n{}\n", b.pos_to_text(fs));

        fs.topology_error(&mut msg)
    }

    fn path_lock_root(&self, paths: &mut Paths<'_>, idx: PathIdx, depth_want: usize) -> Result<(), BchError> {
        let fs = self.fs();

        ebug_assert!(paths[idx].nodes_locked == 0);

        loop {
            let btree = paths[idx].btree_id();
            let packed = root_packed(fs, btree);
            let (b, level) = root_unpack(packed);
            let Some(b) = b else {
                if btree.0 >= fs.btree_id_nr_alive() || !fs.flags.get(c::FsFlag::btree_running) {
                    return Err(fs.err(Errcode::BCH_ERR_btree_not_started));
                }

                let err = fs.btree.cache.roots.error(btree.0);
                ebug_assert!(err != 0);
                /*
                 * Returning 0 here would spin: our caller loops until
                 * the root is locked or we return an error
                 */
                return Err(if err != 0 {
                    BchError::from_raw(err)
                } else {
                    fs.err(Errcode::BCH_ERR_btree_root_error_unset)
                });
            };
            let b = unsafe { b.as_ref() };

            paths[idx].bits.set_level(level as u8);

            if level < depth_want {
                /*
                 * the root is at a lower depth than the depth we want:
                 * got to the end of the btree, or we're walking nodes
                 * greater than some depth and there are no nodes >=
                 * that depth
                 */
                let path = &mut paths[idx];
                path.bits.set_level(depth_want as u8);
                for l in depth_want..c::BTREE_MAX_DEPTH as usize {
                    path.l[l].b = NodePtr::null();
                }
                return Err(Errcode::BCH_ERR_no_btree_node_shallow.into());
            }

            let lock_type = paths[idx].lock_want_six(level);
            self.locking_hash_val.set(0);
            self.locking_root_id.set(btree);

            match self.node_lock(paths, idx, &b.c, level, lock_type) {
                Ok(hold) if packed == root_packed(fs, btree) && !race_fault() => {
                    let b = unsafe { LockedNode::new(b) };
                    if b.contents().data().min_key != POS_MIN || b.contents().key_i().k.p != SPOS_MAX {
                        let err = self.node_root_err(&b);
                        drop(hold);
                        return Err(err);
                    }

                    let path = &mut paths[idx];
                    for l in path.l.iter_mut().take(level) {
                        l.b = NodePtr::err(Errcode::BCH_ERR_no_btree_node_lock_root.into());
                    }
                    for l in level + 1..c::BTREE_MAX_DEPTH as usize {
                        path.l[l].b = NodePtr::null();
                    }
                    path.put_level(level, &b.c, hold);
                    path.lock_taken(level);

                    self.path_level_init(paths, idx, level, b);
                    return Ok(());
                }
                Ok(hold) => drop(hold),
                Err(e) if e == Errcode::BCH_ERR_no_btree_node_reused.into() => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn prefetch_nr(&self, level: usize) -> u32 {
        if self.fs().flags.get(c::FsFlag::started) {
            if level > 1 { 0 } else { 2 }
        } else if level > 1 {
            1
        } else {
            16
        }
    }

    #[cold]
    #[inline(never)]
    fn path_prefetch(&self, paths: &mut Paths<'_>, idx: PathIdx) -> Result<(), BchError> {
        let level = paths[idx].level() as usize;
        // the path holds it locked throughout, through node_prefetch()'s &mut paths
        let b = unsafe { LockedNode::new(paths[idx].level_btree_node(level)) };
        let keys = b.keys(b.contents());
        let mut node_iter = paths[idx].l[level].iter;
        let mut tmp = BkeyBuf::new();

        for _ in 0..self.prefetch_nr(level) {
            assert!(paths[idx].locked(level));

            node_iter.advance(&keys);
            let Some(k) = node_iter.peek(&keys) else { break };

            let mut u = c::bkey::new();
            tmp.reassemble(keys.disassemble(k, &mut u));
            self.node_prefetch(paths, idx, &tmp, level)?;
        }

        Ok(())
    }

    fn node_prefetch(&self, paths: &mut Paths<'_>, idx: PathIdx, k: &BkeyBuf, level: usize) -> Result<(), BchError> {
        let btree = paths[idx].btree_id();
        self.btree_node_prefetch(paths, Some(idx), k.as_ref(), btree, level - 1)
    }

    fn path_prefetch_j(&self, paths: &mut Paths<'_>, idx: PathIdx, jiter: &mut BtreeAndJournalIter<'_, '_>)
        -> Result<(), BchError>
    {
        let level = paths[idx].level() as usize;
        let was_locked = paths[idx].locked(level);
        let mut tmp = BkeyBuf::new();
        let mut ret = Ok(());

        jiter.fail_if_too_many_whiteouts = true;

        for _ in 0..self.prefetch_nr(level) {
            if !self.node_relock(paths, idx, level) {
                break;
            }

            jiter.advance();
            let Some(k) = jiter.peek() else { break };

            tmp.reassemble(k);
            ret = self.node_prefetch(paths, idx, &tmp, level);
            if ret.is_err() {
                break;
            }
        }

        if !was_locked {
            self.node_unlock(paths, idx, level);
        }

        ret
    }

    #[cold]
    #[inline(never)]
    fn node_missing_err(&self, paths: &Paths<'_>, idx: PathIdx) -> BchError {
        let fs = self.fs();
        let path = &paths[idx];
        let mut msg = LogMsg::new(fs);

        prt!(&mut *msg, "node not found at pos: {}", path.pos);
        prt!(&mut *msg, "\n  within parent node ");
        let b = path.locked_btree_node(path.level() as usize).expect("a path goes down from a locked node");
        bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(b.contents().key_i()));
        prt!(&mut *msg, "\n");

        fs.topology_error(&mut msg)
    }

    #[cold]
    #[inline(never)]
    fn node_gap_err(&self, t: &TransAttempt<'_, '_>, paths: &Paths<'_>, idx: PathIdx, k: &BkeyI) -> BchError {
        let fs = self.fs();
        let path = &paths[idx];
        let mut msg = LogMsg::new(fs);

        let level = path.level() as usize;
        let b = path.locked_btree_node(level).expect("a path goes down from a locked node");
        let contents = b.contents();
        let keys = b.keys(contents);
        let v2 = BkeySC::from(k).arm::<c::BtreePtrV2>();
        let have_min = v2.is_some();
        let child_min = v2.map_or(POS_MIN, |p| p.v().min_key);
        let child_max = k.k.p;
        let mut prev_iter = path.l[level].iter;
        let mut next_iter = path.l[level].iter;
        let prev = prev_iter.prev(&keys);
        let cur = next_iter.peek(&keys);
        let next = cur.and_then(|_| {
            next_iter.advance(&keys);
            next_iter.peek(&keys)
        });

        prt!(&mut *msg, "lookup found topology error:\n");
        bch2_btree_id_level_to_text(&mut msg, path.btree_id(), level as c_uint);
        prt!(&mut *msg, " lookup at {}\n", path.pos);

        {
            let mut msg = msg.indent(2);
            prt!(&mut *msg, "parent:     ");
            bch2_btree_id_level_to_text(&mut msg, crate::format::Btree(b.c.btree_id.into()), b.c.level.into());
            prt!(&mut *msg, " {} - {}\n", contents.data().min_key, contents.data().max_key);

            prt!(&mut *msg, "parent key:  ");
            bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(contents.key_i()));
            prt!(&mut *msg, "\nselected:   ");
            bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(k));
            prt!(&mut *msg, "\n");

            prt!(&mut *msg, "parent iter:\n");
            {
                let mut msg = msg.indent(2);
                let mut key = |label: &str, k: Option<u16>| {
                    prt!(&mut *msg, "{}", label);
                    match k {
                        Some(k) => {
                            let mut u = c::bkey::new();
                            let tmp = BkeyBuf::from_key(keys.disassemble(k, &mut u));
                            bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(tmp.as_ref()));
                        }
                        None => prt!(&mut *msg, "(none)"),
                    }
                    prt!(&mut *msg, "\n");
                };
                key("prev:    ", prev);
                key("current: ", cur);
                key("next:    ", next);
            }

            prt!(&mut *msg, "reason:      ");
            if have_min && path.pos < child_min {
                prt!(&mut *msg, "lookup position is before child min_key {}", child_min);
            } else if path.pos > child_max {
                prt!(&mut *msg, "lookup position is after child max_key {}", child_max);
            } else {
                prt!(&mut *msg, "child pointer does not cover lookup position");
            }
            prt!(&mut *msg, "\n");
        }

        let _ = prt_task_backtrace(&mut msg, crate::util::sched::current(), 1);

        let mut msg = msg.indent(2);
        match check_topology_msg(t, b, &mut msg) {
            Err(e) => e,
            Ok(()) => fs.topology_error(&mut msg),
        }
    }

    #[cold]
    #[inline(never)]
    fn node_iter_and_journal_peek(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, idx: PathIdx,
                                  flags: BtreeIterFlags) -> Result<(), BchError>
    {
        let fs = self.fs();
        let path = &paths[idx];
        let level = path.level() as usize;
        let b = paths.held_btree_node(idx, level).expect("a path goes down from a locked node");

        let mut jiter = BtreeAndJournalIter::new(t, b, path.l[level].iter, path.pos);

        let Some(k) = jiter.peek() else {
            return Err(self.node_missing_err(paths, idx));
        };
        set_down_key(paths.down_key_mut(), k);

        if flags.contains(BtreeIterFlags::PREFETCH) && fs.opts.btree_node_prefetch.get() != 0 {
            return self.path_prefetch_j(paths, idx, &mut jiter);
        }

        Ok(())
    }

    #[inline(always)]
    fn path_down(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, idx: PathIdx, flags: BtreeIterFlags)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let level = paths[idx].level() as usize;

        ebug_assert!(paths[idx].locked(level));

        /* an unread node, had under READ_ERRORS, has no keys to go down through */
        if flags.contains(BtreeIterFlags::READ_ERRORS) &&
           paths[idx].locked_btree_node(level).is_some_and(|b| b.flags.read_error()) {
            return Err(fs.err(Errcode::BCH_ERR_btree_node_read_err_cached));
        }

        if self.journal_replay_not_finished() {
            self.node_iter_and_journal_peek(t, paths, idx, flags)?;
        } else {
            let found = {
                let (b, iter, down) = paths.level_and_down_key_mut(idx, level)
                    .expect("a path goes down from a locked node");
                let keys = b.keys(b.contents());
                iter.peek(&keys).map(|k| set_down_key_packed(down, &keys, k)).is_some()
            };
            if !found {
                return Err(self.node_missing_err(paths, idx));
            }

            if flags.contains(BtreeIterFlags::PREFETCH) && fs.opts.btree_node_prefetch.get() != 0 {
                self.path_prefetch(paths, idx)?;
            }
        }

        if !bpos_in_btree_node_key(paths[idx].pos, paths.down_key()) {
            return Err(self.node_gap_err(t, paths, idx, paths.down_key()));
        }

        let level = level - 1;
        let lock_type = paths[idx].lock_want_six(level);
        // the transaction's, not the path table's
        let k: *const BkeyI = paths.down_key();
        let (b, hold) = self.btree_node_get(paths, idx, unsafe { &*k }, level, lock_type, flags)?;
        let b = unsafe { LockedNode::new(b) };

        let path = &mut paths[idx];
        path.put_level(level, &b.c, hold);
        path.lock_taken(level);
        path.bits.set_level(level as u8);
        self.path_level_init(paths, idx, level, b);

        self.verify_path_locks(paths, idx);
        Ok(())
    }

    pub(crate) fn path_traverse_all(&self, paths: &mut Paths<'_>) -> Result<(), BchError> {
        if self.in_traverse_all() {
            return Err(self.restart(Errcode::BCH_ERR_transaction_restart_in_traverse_all));
        }

        /*
         * XXX: when restart reason is transaction_restart_lock_waitlist_alloc,
         * we should wait for lock contention to drop (FIFO to drain) before
         * retrying, otherwise we'll just keep hammering the same full waitlist.
         * Currently there's no mechanism to hook back-off on a specific lock;
         * adding one is nontrivial. For now we just retry and rely on timing /
         * other waiters completing to give us room.
         */
        self.set_in_traverse_all(true);

        let ret = 'retry_all: loop {
            self.set_restarted(c::Errcode(0));
            self.last_restarted_ip.set(0);

            for i in self.path_idxs() {
                paths[i].bits.set_should_be_locked(false);
            }

            paths.sort();

            self.unlock_from(paths, crate::ret_ip!());
            self.mark_locked(false);

            if self.memory_allocation_failure() {
                stack_pin_init!(let cl = StackClosure::new());
                loop {
                    let ret = self.btree_cache_cannibalize_lock(NonNull::new(cl.as_ptr()));
                    cl.sync();
                    if ret.is_ok() {
                        break;
                    }
                }
            }

            /*
             * Now, redo traversals in correct order. Each pass is an attempt of
             * its own: one from before a retry would be stale, and a key cache
             * fill relocking against it would restart forever.
             */
            let t = TransAttempt::new(self, self.restart_count());
            let mut i = 0;
            while i < paths.sorted().len() {
                let idx = paths.sorted()[i];
                let path = &paths[idx];

                /*
                 * Traversing a path can cause another path to be added at about
                 * the same position:
                 */
                if path.nodes_locked == 0 && !path.node(path.level() as usize).is_null() {
                    paths.get_ll(idx, false);
                    let ret = self.path_traverse_one(&t, paths, idx, BtreeIterFlags::empty());
                    paths.put_ll(idx, false);

                    match ret {
                        Err(e) if e.matches(Errcode::BCH_ERR_transaction_restart) ||
                                  e.matches(c::ENOMEM as i32) => continue 'retry_all,
                        Err(e) => break 'retry_all Err(e),
                        Ok(()) => {}
                    }
                } else {
                    i += 1;
                }
            }

            /*
             * We used to assert that all paths had been traversed here
             * (all paths uptodate); however, since
             * path->should_be_locked is not set yet, we might have unlocked and
             * then failed to relock a path - that's fine.
             */
            break Ok(());
        };

        self.btree_cache_cannibalize_unlock();

        self.set_in_traverse_all(false);
        ret
    }

    fn path_good_node(&self, paths: &mut Paths<'_>, idx: PathIdx, l: usize, check_pos: Ordering) -> bool {
        paths[idx].is_btree_node(l) &&
            self.node_relock(paths, idx, l) &&
            paths[idx].check_pos_in_node(l, check_pos)
    }

    #[inline(never)]
    fn path_up_until_good_node_slowpath(&self, paths: &mut Paths<'_>, idx: PathIdx, check_pos: Ordering) -> usize {
        let mut l = paths[idx].level() as usize;

        'again: loop {
            while !paths[idx].node(l).is_null() && !self.path_good_node(paths, idx, l, check_pos) {
                self.path_set_level_up(paths, idx, l);
                l += 1;
            }

            /* If we need intent locks, take them too: */
            let mut i = l + 1;
            while i < paths[idx].locks_want() as usize && !paths[idx].node(i).is_null() {
                if !self.node_relock(paths, idx, i) {
                    while l <= i {
                        self.path_set_level_up(paths, idx, l);
                        l += 1;
                    }
                    continue 'again;
                }
                i += 1;
            }

            return l;
        }
    }

    #[inline(always)]
    pub(crate) fn path_up_until_good_node(&self, paths: &mut Paths<'_>, idx: PathIdx, check_pos: Ordering) -> usize {
        let path = &paths[idx];
        let level = path.level() as usize;
        let l = if path.locked(level) && path.check_pos_in_node(level, check_pos) {
            level
        } else {
            self.path_up_until_good_node_slowpath(paths, idx, check_pos)
        };
        // node() is NULL at BTREE_MAX_DEPTH: the walk up stops there at most
        invariant!(l <= c::BTREE_MAX_DEPTH as usize);
        l
    }

    /// This is the main state machine for walking down the btree - walks down
    /// to a specified depth
    ///
    /// On error, caller (peek_node()/peek_key()) must return NULL; the error is
    /// stashed in the iterator and returned from bch2_trans_exit().
    ///
    /// Cached and btree paths each get a function of their own: a key cache
    /// fill traverses a btree path from inside the cached traverse, and that
    /// shouldn't be under the btree walk's frame.
    #[inline]
    pub(crate) fn path_traverse_one(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, idx: PathIdx,
                                    flags: BtreeIterFlags) -> Result<(), BchError>
    {
        if paths[idx].cached() {
            self.path_traverse_cached(t, paths, idx, flags)
        } else {
            self.path_traverse_btree(t, paths, idx, flags)
        }
    }

    #[inline(never)]
    fn path_traverse_cached(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, idx: PathIdx,
                            flags: BtreeIterFlags) -> Result<(), BchError>
    {
        let ret = self.path_traverse_start(paths, idx)
            .unwrap_or_else(|| self.btree_path_traverse_cached(t, paths, idx, flags));
        self.path_traverse_done(paths, idx, &ret);
        ret
    }

    #[inline(never)]
    fn path_traverse_btree(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, idx: PathIdx,
                           flags: BtreeIterFlags) -> Result<(), BchError>
    {
        let depth_want = paths[idx].level() as usize;
        let ret = self.path_traverse_start(paths, idx)
            .unwrap_or_else(|| self.path_walk_down(t, paths, idx, flags, depth_want));
        self.path_traverse_done(paths, idx, &ret);
        ret
    }

    fn path_traverse_done(&self, paths: &mut Paths<'_>, idx: PathIdx, ret: &Result<(), BchError>) {
        ebug_assert!(ret.as_ref().is_err_and(|e| e.matches(Errcode::BCH_ERR_transaction_restart)) ==
                     (self.restarted().0 != 0));
        self.path_verify(paths, idx);
    }

    /// What every traverse checks first: the traverse's result, if that's
    /// the end of it.
    #[inline(always)]
    fn path_traverse_start(&self, paths: &mut Paths<'_>, idx: PathIdx) -> Option<Result<(), BchError>> {
        if self.restarted().0 != 0 {
            return Some(Err(BchError::from(self.restarted())));
        }

        if !self.srcu_held() {
            self.srcu_lock();
        }

        self.fs().event_trace(events::btree_path_traverse_start, |out| {
            prt!(out, "{}\n", self.fn_name());
            self.path_to_text(out, paths, idx);
        });

        if self.path_relock_norestart(paths, idx) {
            return Some(Ok(()));
        }

        /*
         * Ensure we obey path->should_be_locked: if it's set, we can't unlock
         * and re-traverse the path without a transaction restart:
         */
        if paths[idx].should_be_locked() {
            return Some(Err(self.restart(Errcode::BCH_ERR_transaction_restart_relock_path)));
        }

        None
    }

    fn path_walk_down(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, idx: PathIdx, flags: BtreeIterFlags,
                      depth_want: usize) -> Result<(), BchError> {
        if paths[idx].level() as u32 >= c::BTREE_MAX_DEPTH {
            self.path_traverse_end_trace(paths, idx);
            return Ok(());
        }

        let level = self.path_up_until_good_node(paths, idx, Ordering::Equal);
        paths[idx].bits.set_level(level as u8);
        let max_level = level;

        ebug_assert!(paths[idx].node(level).is_null() || paths[idx].locked(level));

        /*
         * Note: path->nodes[path->level] may be temporarily NULL here - that
         * would indicate to other code that we got to the end of the btree,
         * here it indicates that relocking the root failed - it's critical that
         * btree_path_lock_root() comes next and that it can't fail
         */
        while paths[idx].level() as usize > depth_want {
            let level = paths[idx].level() as usize;
            let ret = if !paths[idx].node(level).is_null() {
                self.path_down(t, paths, idx, flags)
            } else {
                self.path_lock_root(paths, idx, depth_want)
            };

            if let Err(e) = ret {
                if e == Errcode::BCH_ERR_no_btree_node_shallow.into() {
                    /*
                     * No nodes at this level - got to the end of
                     * the btree:
                     */
                    return Ok(());
                }

                self.path_unlock(paths, idx);
                let path = &mut paths[idx];
                path.bits.set_level(depth_want as u8);
                path.l[depth_want].b = NodePtr::err(e);
                return Err(e);
            }
        }

        let level = paths[idx].level() as usize;
        if max_level > level {
            for linked in self.path_idxs() {
                let at_node = linked != idx && {
                    let b = paths[idx].locked_btree_node(level).expect("a traversed path's level is locked");
                    paths[linked].has_node(level, &b.c, b.c.lock.six().seq.get()) && paths[linked].pos_in_node(b)
                };
                if !at_node {
                    continue;
                }

                for j in level + 1..max_level {
                    let l = paths[idx].l[j];
                    paths[linked].l[j] = l;
                }
            }
        }

        self.path_traverse_end_trace(paths, idx);
        Ok(())
    }

    fn path_traverse_end_trace(&self, paths: &Paths<'_>, idx: PathIdx) {
        self.fs().event_trace(events::btree_path_traverse_end, |out| {
            prt!(out, "{}\n", self.fn_name());
            self.path_to_text(out, paths, idx);
        });
    }
}

// ── For C ───────────────────────────────────────────────────────────────────

