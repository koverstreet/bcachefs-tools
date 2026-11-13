// SPDX-License-Identifier: GPL-2.0

//! Async btree ops: a node rewritten, on a read error, or merged, when it's
//! nearly empty - run from a workqueue, not where the need was noticed.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering::{AcqRel, Acquire}};
use cstruct_macros::{Init, Late};
use pin_init::pin_data;
use crate::btree::iter::{lockrestart_do, BtreeIterFlags, BtreeNodeIter, BtreeNodeRef, CommitOpts, TransAttempt};
use crate::btree::node::{BtreeNode, NodeContents};
use crate::c;
use crate::c::bch_fs_flags as Flag;
use crate::data_structures::cuckoo::{Cuckoo, Entry, Registration};
use crate::errcode::BchError;
use crate::format::{Bpos, Btree};
use crate::fs::{Fs, WriteRef};
use crate::init::passes::RecoveryPass;
use crate::locking::{Locked, MemallocFlags};
use crate::new_lock;
use crate::util::alloc::{AllocError, Kmalloc, KVVec};
use crate::util::backref::BackRef;
use crate::util::futex::EventCount;
use crate::util::late::Late;
use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::rcu::Detach;
use crate::util::refcount::TryRefcount;
use crate::util::sleep::Sleep;
use crate::util::task::spawn_static;
use crate::util::workqueue::{Workqueue, WqFlags};

/*
 * Ops are small and deduped, but a large fsck can touch millions of nodes -
 * don't let deferred housekeeping pile up without bound. The triggers
 * re-fire until the work actually happens, so a dropped op is re-noticed
 * later; for the same reason allocation failure is fine to eat - dropping
 * the op IS the memory-pressure response.
 */
const ASYNC_BTREE_OPS_MAX: usize = 8192;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AsyncBtreeOp {
    Rewrite,
    Merge,
    MergeNoRead,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct NodePos {
    btree: Btree,
    level: u32,
    pos:   Bpos,
}

type OpTable = Cuckoo<Entry<NodePos>>;
type OpBox = MaybeRcuAlloc<Op, Kmalloc>;

/*
 * Async btree ops (rewrite on read error, merge of an underfull node)
 * identify their node by position plus the node's random cookie
 * (bch_btree_ptr_v2.seq) instead of holding the node key: a fresh cookie is
 * generated for every node allocation, so if the node has been rewritten or
 * freed since the op was queued the seq won't match and the op is a no-op.
 */
pub(crate) struct Op {
    /* its key in the table, while it's in */
    pos: NodePos,
    op:  AsyncBtreeOp,
    seq: u64,
    reg: Option<Registration<NodePos, NodePos>>,
}

impl Detach for Op {
    fn detach(&self) {
        if let Some(r) = &self.reg {
            r.detach();
        }
    }
}

impl Op {
    fn run(&self, t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
        let NodePos { btree, level, pos } = self.pos;
        /*
         * merge_no_read: only merge if the node is still cached - a very
         * empty node seen at read time isn't worth reading back in for,
         * and ops parked during recovery may not run until long after.
         */
        let flags = if self.op == AsyncBtreeOp::MergeNoRead { BtreeIterFlags::NOFILL } else { BtreeIterFlags::empty() };
        let mut iter = BtreeNodeIter::new(t, btree, pos, c::BTREE_MAX_DEPTH, level, flags);
        let Some(b) = iter.peek(t)?
            .filter(|b| b.contents().data().keys.seq.get() == self.seq)
            .map(BtreeNodeRef::from)
        else {
            return Err(BchError::from(c::ENOENT));
        };

        if self.op == AsyncBtreeOp::Rewrite {
            t.btree_node_rewrite(iter.iter(), b, 0, CommitOpts::new(), c::bch_write_flags::empty())
        } else {
            let trans = t.trans();
            super::interior::foreground_maybe_merge(t, &mut trans.paths(), iter.iter().path, level as usize,
                                                    CommitOpts::new(), None)
        }
    }
}

#[pin_data]
#[derive(Init, Late)]
pub(crate) struct AsyncOps {
    /* Every op, queued, running or pending: one per node */
    table: OpTable,
    /* Queued before the filesystem may go read-write: run_pending_async_btree_ops() runs them */
    #[pin]
    #[init(new_lock!(KVVec::new()))]
    pending: Locked<c::mutex, KVVec<OpBox>>,
    running: AtomicU32,
    running_done: EventCount,
    #[late(Workqueue::new(c"btree_node_rewrite", WqFlags::PERCPU, 0), ENOMEM_btree_interior_update_worker_init)]
    worker: Late<Workqueue>,
}

impl AsyncOps {
    fn op_done(&self) {
        self.running.fetch_sub(1, AcqRel);
        self.running_done.wake();
    }

    /// The ops queued before the filesystem could go read-write, dropped.
    pub(crate) fn free_pending(&self) {
        self.pending.lock().clear();
    }

    /// Until no op is running.
    pub fn flush(&self) {
        let Ok(()) = self.running_done.wait_until(Sleep::UNINTERRUPTIBLE, || {
            (self.running.load(Acquire) == 0).then_some(())
        });
    }
}

async fn task(_write: WriteRef, fs: BackRef<Fs>, a: OpBox) {
    let fs = &*fs;
    let trans = crate::btree_trans!(fs);
    if let Err(e) = lockrestart_do(&trans, |t| a.run(t)) {
        if !e.matches(c::ENOENT) && !e.matches(c::EROFS) &&
           !e.matches(c::bch_errcode::BCH_ERR_no_btree_node_nofill) {
            crate::bch_err_ratelimited!(fs, "{}: error {}", crate::here_str!(), e.msg());
        }
    }
    drop(trans);
    drop(a);

    /* async_btree_op() registers under btree locks, where it can't grow the table: */
    let _ = fs.btree.async_ops.table.make_room();

    fs.btree.async_ops.op_done();
}

impl Fs {
    /// @a run, counted in running already: under the lock that took it, for
    /// flush to see it.
    fn async_btree_op_start(&self, write: WriteRef, a: OpBox) {
        // the task holds it, and its write ref: going read-only waits for that
        let fs_ref = unsafe { BackRef::new(self) };
        let fs = unsafe { fs_ref.get() };

        if spawn_static(&fs.btree.async_ops.worker, move || task(write, fs_ref, a)).is_err() {
            self.btree.async_ops.op_done();
        }
    }

    /// @op on @b, from a workqueue - or once the filesystem goes read-write,
    /// if it isn't yet. Dropped if it's queued for @b already, or too many
    /// are.
    pub(crate) fn async_btree_op(&self, b: &BtreeNode, contents: &NodeContents, op: AsyncBtreeOp) {
        let r = &self.btree.async_ops;
        let pos = NodePos { btree: Btree(b.c.btree_id.into()), level: b.c.level.into(), pos: contents.key.k.p };

        /*
         * Dedup: the triggers fire repeatedly (e.g. every write buffer key
         * flushed into a node that needs merging) until the op actually runs
         * - only one op per node may be queued.
         */
        if r.table.len() >= ASYNC_BTREE_OPS_MAX || r.table.contains(&pos) {
            return;
        }

        let _noio = MemallocFlags::noio();
        let seq = contents.data().keys.seq.get();
        let Ok(mut a) = OpBox::new(::pin_init::init!(Op { pos, op, seq, reg: None }? AllocError)) else { return };
        let reg = {
            let _noreclaim = MemallocFlags::noreclaim();
            unsafe { Registration::register(&r.table, pos, NonNull::from(&a.pos)) }
        };
        let Ok(reg) = reg else { return };
        a.as_pin_mut().get_mut().reg = Some(reg);

        let mut pending = r.pending.lock();
        let replay_done = self.recovery.runner.passes_complete.get() & 1 << RecoveryPass::journal_replay as u32 != 0;
        if let Some(write) = replay_done.then(|| self.writes.tryget_live()).flatten() {
            r.running.fetch_add(1, AcqRel);
            drop(pending);
            self.async_btree_op_start(write, a);
        } else if !self.flags.get(Flag::may_go_rw) {
            /* run_pending_async_btree_ops() will run it - or, if it can't be held, it's dropped */
            let _ = pending.push(a);
        }
    }

    /// The ops queued before the filesystem could go read-write, run.
    pub(crate) fn run_pending_async_btree_ops(&self) {
        let r = &self.btree.async_ops;
        loop {
            let a = {
                let mut pending = r.pending.lock();
                let Some(a) = pending.pop() else { return };
                r.running.fetch_add(1, AcqRel);
                a
            };
            let Some(write) = self.writes.tryget_live() else {
                drop(a);
                r.op_done();
                return;
            };
            self.async_btree_op_start(write, a);
        }
    }
}
