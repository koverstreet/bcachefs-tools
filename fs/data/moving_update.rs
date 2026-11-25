// SPDX-License-Identifier: GPL-2.0

//! A mover's data update: a data update's task (update_task.rs), resumed on
//! the moving context's run queue. The run queue runs only in the mover's
//! waits, so an update's steps run on the mover's thread between its own: a
//! mover can't issue faster than it completes, and its work stays out of
//! everyone else's way. Its write is allocated there too, blocking the mover
//! when the allocator is - backpressure; its write's completion, the IO and
//! the index update, runs where every write's does, as the write point's
//! buckets are put there.
//!
//! Writes start in the order the reads were issued: a mover walks keys in
//! order, and data written in order is laid out together. An update's turn is
//! a Gather it waits on before starting its write, whose ref the context's
//! updates fifo holds till every update issued before it has started its own
//! (moving.rs).
//!
//! A read leaves the context's read counts when its task sees it done, and is
//! counted as a write once its write starts; in between, the fifo is what
//! flush_all() waits on.

use crate::btree::bkey::{BkeySC, BkeyV};
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::c;
use crate::c::Errcode;
use crate::data::moving::{Moving, MovingIo};
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::scrub_journal::BadRef;
use crate::data::update::{read_err_benign, IOPRIO_IDLE};
use crate::errcode::BchError;
use crate::data::update_task::{self, Body, Update};
use crate::util::refcount::RefGuard;
use crate::util::task::{requeue, Resume, Spawn};
use crate::data::read::{read_update, ReadDone};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::args_stored_once;

/// Counts the read's errors in the context's stats. For a scrub, a poisoned
/// extent is data already lost even if it reads back clean, as moving it gives
/// the bad data a new checksum. Not for the journal scrub, where uncorrected
/// means a rewind, which can't bring it back.
fn read_error_stats(u: &c::data_update, read: &ReadDone, moving: &Moving<'_>, size: u32) {
    let stats = moving.stats();
    let fs = u.op.fs();
    let k = read.poisoned.as_deref().map_or(BkeySC::from(u.index.k.as_ref()), BkeySC::from);
    let poisoned = u.index.opts.type_ == c::DataUpdateType::scrub &&
        k.arm::<Ptrs>().is_some_and(|k| k.poisoned(fs));

    let ret = read.ret;
    if ret.is_err_and(read_err_benign) {
        // nothing was read, and nothing was lost
    } else if ret.is_err() || poisoned {
        stats.sectors_error_uncorrected.fetch_add(size.into(), Relaxed);
    } else if u.rbio.saw_error.load(Relaxed) {
        stats.sectors_error_corrected.fetch_add(size.into(), Relaxed);
    }
}

/// The body, @seq in its context's updates: reads, counted in the context as
/// @read_io; then waits for its turn, then writes - nothing, if the read
/// failed. @bucket counts the update against the bucket it's moving data off,
/// until it's done. A journal scrub's update puts what it finds bad to @found.
/// @runq is the context's run queue.
#[args_stored_once]
async fn run(mut u: Body, seq: u64, read_io: MovingIo, bucket: Option<RefGuard<c::move_bucket>>,
             found: Option<BadRef>, runq: Resume<'_>) {
    let read = {
        let d = &mut *u;
        read_update(d.op.fs(), &mut d.rbio, d.index.k.as_ref(), d.index.btree_id, d.index.opts.read_flags,
                    d.index.opts.read_dev, runq).await
    };
    drop(read_io);
    u.read_done.store(true, Relaxed);

    u.turn().wait(runq).await;
    // the context's updates are its thread's: there, if the turn was ours
    // already and the wait didn't move us - a read's retry ends where it ran
    requeue(runq).await;

    let write_io = {
        let moving = u.index.moving().expect("a mover's update has its context");
        let size = u.index.k.as_ref().k().size;
        moving.update_write_start(seq);
        read_error_stats(&u, &read, moving, size);
        moving.io_start(true, size)
    };
    // A device the extent's on is offline: reconcile retrying the move can't
    // read it till that device is back, so it's parked till then. Not an IO
    // error, which a retry may get past, and whose device needn't change.
    let unreadable = u.index.opts.type_ == c::DataUpdateType::reconcile &&
        read.ret.is_err_and(|e| e.matches(Errcode::no_device_to_read_from));
    let ret = u.write(read, found.as_ref(), runq).await;
    drop(write_io);

    if unreadable {
        crate::data::update_index::no_device_to_read_from(&u);
    }

    // EC allocation failed: the write never happened, but the extent still
    // needs erasure coding, so mark it pending for reconcile to retry. Not for
    // the transient failures: nothing retries the pending list on its own, so
    // parking work a retry would have done abandons it
    let ret = match ret {
        Err(e) if e.matches(Errcode::ec_alloc_failed) && !e.matches(Errcode::ec_alloc_failed_transient) =>
            Err(crate::data::update_index::ec_alloc_failed(&u, e)),
        ret => ret,
    };
    // off the context's updates once exit() has its nocow locks gone, as what
    // waits for those waits on the context
    let ctxt = u.index.ctxt.take();
    u.exit(ret);
    let moving = Moving::of(ctxt.as_deref().expect("a mover's update has its context"));
    moving.update_done(seq);
    moving.wake();
    drop(bucket);
}

/// Starts moving extent @k: its task, which issues the read - or nothing, if
/// there's nothing to read.
pub(crate) fn move_extent(moving: &Moving<'_>, t: &TransAttempt<'_, '_>, bucket_in_flight: Option<&c::move_bucket>,
                          iter: &BtreeIter<'_>, k: BkeySC<'_>, io_opts: &c::bch_inode_opts,
                          data_opts: &c::data_update_opts, found: Option<BadRef>) -> Result<(), BchError> {
    let btree = iter.btree();

    t.allocate_dropping_locks(|| moving.updates_reserve().map_err(|_| c::ENOMEM.into()))
        .map_err(|e| if e.matches(c::ENOMEM) { t.fs().err(Errcode::ENOMEM_move_extent) } else { e })?;

    // SAFETY: flush_all() waits for every task before the context goes
    let task = unsafe {
        update_task::spawn(t, Spawn::Run(moving.runq()), t.fs().tasks(), Errcode::ENOMEM_move_extent)
    }?;
    // On error init() has cleaned up, and the task is dropped unstarted
    let mut u = match task.init(t, Some(iter), Some(moving), moving.write_point(), io_opts, *data_opts, btree, k) {
        Ok(u) => u,
        Err(e) if e.matches(Errcode::data_update_done) => return Ok(()),
        Err(e) => return Err(e),
    };
    u.rbio.bio.get_mut().set_ioprio(IOPRIO_IDLE);
    let seq = moving.update_issue(u.turn().get_pinned());

    let size = BkeySC::from(u.index.k.as_ref()).k.size;
    let read_io = moving.io_start(false, size);
    let bucket = bucket_in_flight.map(|b| unsafe { RefGuard::get(b) });
    let runq = Resume::run_queue(moving.runq());

    u.start(|b| async move |_: &Update| run(b, seq, read_io, bucket, found, runq).await);

    moving.stats().keys_moved.fetch_add(1, Relaxed);
    moving.stats().sectors_moved.fetch_add(size.into(), Relaxed);
    Ok(())
}
