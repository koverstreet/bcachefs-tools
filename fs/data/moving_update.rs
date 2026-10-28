// SPDX-License-Identifier: GPL-2.0

//! A mover's data update: an UpdateTask (update_task.rs) on the moving
//! context's run queue. The run queue runs only in the mover's waits, so
//! updates run on the mover's thread between its own steps: a mover can't
//! issue faster than it completes, and its work stays out of everyone else's
//! way.
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
use crate::c::bch_errcode;
use crate::data::moving::{Moving, MovingIo};
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::update::{read_err_benign, IOPRIO_IDLE};
use crate::errcode::BchError;
use crate::data::update_task::{Body, Update, UpdateTask};
use crate::util::refcount::{RefGuard, Refcount};
use crate::util::alloc::gfp_flags;
use crate::util::task::{Resumable, Resume, Spawn};
use core::ffi::{c_int, c_uint};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_extern;

c_extern! {
    fn __bch2_read_extent(trans: *mut c::btree_trans, rbio: *mut c::bch_read_bio, iter: c::bvec_iter,
                          read_pos: c::bpos, btree: c::btree_id, k: c::bkey_s_c, offset_into_extent: c_uint,
                          failed: *mut c::bch_io_failures, flags: c::bch_read_flags, dev: c_int) -> c_int;
}

fn moving_size(u: &c::data_update) -> (&Moving<'_>, u32) {
    (u.moving().expect("a mover's update has its context"), u.k.key().k().size)
}

/// Counts the read's errors in the context's stats. For a scrub, a poisoned
/// extent is data already lost even if it reads back clean, as moving it gives
/// the bad data a new checksum. Not for the journal scrub, where uncorrected
/// means a rewind, which can't bring it back.
fn read_error_stats(u: &c::data_update, moving: &Moving<'_>, size: u32) {
    let Some(stats) = moving.stats_opt() else { return };
    let fs = u.op.fs();
    let k = BkeySC::from(u.k.key());
    let poisoned = u.opts.type_ == c::bch_data_update_types::BCH_DATA_UPDATE_scrub &&
        k.arm::<Ptrs>().is_some_and(|k| {
            k.extent_flags(fs) & (1 << c::bch_extent_flags_e::BCH_EXTENT_FLAG_poisoned.0) != 0
        });

    // Not rbio.bvec_iter: only set when the read went straight into this rbio
    let ret = crate::errcode::ret_to_result_void(u.rbio.ret.into());
    if ret.is_err_and(read_err_benign) {
        // nothing was read, and nothing was lost
    } else if ret.is_err() || poisoned {
        stats.sectors_error_uncorrected.fetch_add(size.into(), Relaxed);
        stats.devs_error_uncorrected.assign(u.rbio.pick.ptr.dev().into(), true);
    } else if u.rbio.bits().saw_error() != 0 {
        stats.sectors_error_corrected.fetch_add(size.into(), Relaxed);
    }
}

/// The body: waits for the read, counted in the context as @read_io, then for
/// its turn, then writes. @bucket counts the update against the bucket it's
/// moving data off, until it's done.
async fn run(mut u: Body, read_io: MovingIo, bucket: Option<RefGuard<c::move_bucket>>) {
    u.read().await;
    drop(read_io);

    u.turn().wait().resume_on(Resume::Spawned).await;

    let write_io = {
        let (moving, size) = moving_size(&u);
        moving.update_write_start(u.io_seq);
        read_error_stats(&u, moving, size);
        moving.io_start(true, size)
    };
    u.write().await;
    drop(write_io);

    // EC allocation failed: the write never happened, but the extent still
    // needs erasure coding, so mark it pending for reconcile to retry. Not for
    // the transient failures: nothing retries the pending list on its own, so
    // parking work a retry would have done abandons it
    let err = c_int::from(u.op.error);
    if let Err(e) = crate::errcode::ret_to_result_void(err) {
        if e.matches(bch_errcode::BCH_ERR_ec_alloc_failed) &&
           !e.matches(bch_errcode::BCH_ERR_ec_alloc_failed_transient) {
            crate::data::update_index::ec_alloc_failed(&mut u);
        }
    }
    u.exit(err);
    drop(bucket);
}

/// # Safety
/// @bio is a mover's update's read bio.
unsafe extern "C" fn move_read_endio(bio: *mut c::bio) {
    // SAFETY: the caller's; the bio is in the rbio, which is in the update
    unsafe {
        let rbio = bio.byte_sub(core::mem::offset_of!(c::bch_read_bio, bio)).cast::<c::bch_read_bio>();
        Update::read_done(rbio.byte_sub(core::mem::offset_of!(c::data_update, rbio)).cast());
    }
}

/// Starts moving extent @k, returning once the read is issued - or at once if
/// there's nothing to read.
pub(crate) fn move_extent(moving: &Moving<'_>, t: &TransAttempt<'_, '_>, bucket_in_flight: Option<&c::move_bucket>,
                          iter: &BtreeIter<'_>, k: BkeySC<'_>, io_opts: &c::bch_inode_opts,
                          data_opts: &c::data_update_opts) -> Result<(), BchError> {
    let btree = iter.btree();

    if let Some(stats) = moving.stats_opt() {
        stats.set_pos(c::bbpos { btree, pos: iter.pos() });
    }

    t.allocate_dropping_locks(|gfp| moving.updates_reserve(gfp_flags(gfp)).map_err(|_| c::ENOMEM.into()))
        .map_err(|e| if e.matches(c::ENOMEM) { t.fs().err(bch_errcode::BCH_ERR_ENOMEM_move_extent) } else { e })?;

    // SAFETY: flush_all() waits for every task before the context goes
    let task = unsafe {
        UpdateTask::spawn(t, Spawn::Run(moving.runq()), bch_errcode::BCH_ERR_ENOMEM_move_extent)
    }?;
    // On error init() has cleaned up, and the task is dropped unstarted
    let mut u = match task.init(t, Some(iter), Some(moving), moving.write_point(), io_opts, *data_opts, btree, k) {
        Ok(u) => u,
        Err(e) if e.matches(bch_errcode::BCH_ERR_data_update_done) => return Ok(()),
        Err(e) => return Err(e),
    };
    u.rbio.bio.bi_end_io = Some(move_read_endio);
    u.rbio.bio.bi_ioprio = IOPRIO_IDLE;
    u.io_seq = moving.update_issue(u.turn().get());

    let d = &mut **u;
    let key = BkeySC::from(d.k.key());
    let size = key.k.size;
    let read_io = moving.io_start(false, size);
    let bucket = bucket_in_flight.map(Refcount::get);

    // SAFETY: we're in the attempt, and init() set the read bio up
    let ret = unsafe {
        __bch2_read_extent(t.raw(), &mut d.rbio, d.rbio.bio.bi_iter, key.k.start_pos(), btree, key.into(), 0,
                           core::ptr::null_mut(), data_opts.read_flags | c::bch_read_flags::BCH_READ_last_fragment,
                           data_opts.read_dev as c_int)
    };
    if let Err(e) = t.result(ret) {
        // __bch2_read_extent() fails synchronously only before it touches the
        // rbio - a restart, or ENOMEM setting up a promote - so the endio won't
        // run
        u.abandon(ret);
        return Err(e);
    }

    u.start(|b| run(b, read_io, bucket));

    if let Some(stats) = moving.stats_opt() {
        stats.keys_moved.fetch_add(1, Relaxed);
        stats.sectors_moved.fetch_add(size.into(), Relaxed);
    }
    Ok(())
}
