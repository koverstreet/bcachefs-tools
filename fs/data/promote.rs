// SPDX-License-Identifier: GPL-2.0

//! Promotes: when a read's data should also be on the promote target - a
//! cache - a data update writes a cached copy there. The self-heal is the
//! same for a read with IO errors: it rewrites the replicas that failed.
//! Promotes are opportunistic, so they're skipped when they can't be done
//! cheaply: when the target is congested, or too many are in flight.
//!
//! A promote is a data update's task (update_task.rs) on promote_wq.
//! promote_alloc(), called partway through a read, decides whether to
//! promote, and if so makes the update and returns its read bio: a bounce
//! split of the read, for the read path to read into. The update's task is
//! made but not started: the read path starts it when it frees the split.
//!
//! The in-flight limit is one count for the filesystem, PROMOTES_PER_CPU per
//! online CPU.

use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{BkeySC, BkeyV};
use crate::btree::iter::{is_restart, TransAttempt};
use crate::c;
use crate::c::Errcode;
use crate::data::extents::BkeyVExtents;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::read::Rbio;
use crate::data::update_task::{self, AwaitingRead, Body, Update};
use crate::debug::async_objs::AsyncObjSlot;
use crate::errcode::BchError;
use crate::fs::{Fs, WriteRef};
use crate::prt;
use crate::debug::trace::events;
use crate::util::backref::BackRef;
use crate::os::kernel::{current_id, local_clock, num_online_cpus};
use crate::util::refcount::TryRefcount;
use crate::util::time_stats::TimeStats;
use crate::util::task::Spawn;
use crate::util::workqueue::Queue;
use core::ffi::c_int;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::args_stored_once;
use crate::format::{Btree, BkeyType};

/// The in-flight limit, per online CPU.
const PROMOTES_PER_CPU: u32 = 32;

// ── The limit ────────────────────────────────────────────────────────────

impl Fs {
    pub fn promote_limit_tryget(&self) -> bool {
        let max = PROMOTES_PER_CPU * num_online_cpus();
        self.promotes_in_flight
            .fetch_update(Relaxed, Relaxed, |n| (n < max).then_some(n + 1))
            .is_ok()
    }

    pub fn promote_limit_put(&self) {
        let n = self.promotes_in_flight.fetch_sub(1, Relaxed);
        assert!(n != 0, "promote limit put with no promote in flight");
    }

    fn promote_wq(&self) -> &Queue {
        &self.promote_wq
    }
}

/// What a promote holds while in flight: a write ref, so the filesystem
/// doesn't go read-only under it, and its place under the limit.
struct InFlight {
    /// Valid while @_write is held, and it's dropped after drop() runs
    fs:     BackRef<Fs>,
    _write: WriteRef,
}

impl InFlight {
    fn tryget(fs: &Fs) -> Result<Self, BchError> {
        let write = fs.writes.tryget_live().ok_or_else(|| fs.err(Errcode::BCH_ERR_nopromote_no_writes))?;
        if !fs.promote_limit_tryget() {
            return fs.throw(Errcode::BCH_ERR_nopromote_ratelimited);
        }
        // SAFETY: the filesystem outlives the write ref, which going read-only
        // waits for
        Ok(InFlight { fs: unsafe { BackRef::new(fs) }, _write: write })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.fs.promote_limit_put();
    }
}

// ── The promote ──────────────────────────────────────────────────────────

/// The body, started when the read's done: the write. @in_flight is held until
/// after exit().
#[args_stored_once]
async fn run(mut u: Body, in_flight: InFlight, start_time: u64) {
    let fs = u.op.fs();
    let listed = unsafe { AsyncObjSlot::new(fs, &u.rbio) };

    let read = u.read();
    u.read_done.store(true, Relaxed);
    if read.ret.is_err() {
        drop(listed);
        return u.exit(-(c::EIO as c_int));
    }
    {
        let k = BkeySC::from(u.k.key());
        fs.event_add_trace(events::data_read_promote, k.k.size.into(), |out| prt!(out, "{}", k.to_text(fs)));
    }

    u.write(read).await;
    fs.times[c::TimeStat::data_promote as usize].update(start_time);
    drop(listed);
    u.exit(0);
    drop(in_flight);
}

// ── Making one ───────────────────────────────────────────────────────────

fn should_promote(fs: &Fs, k: BkeySC<'_>, opts: &c::bch_inode_opts) -> Result<(), BchError> {
    assert!(opts.promote_target != 0, "should_promote() with no promote target");

    if k.arm::<Ptrs>().is_some_and(|k| k.has_target(fs, Target::decode(opts.promote_target.into()))) {
        fs.event_inc(events::data_read_nopromote_already_promoted);
        return fs.throw(Errcode::BCH_ERR_nopromote_already_promoted);
    }

    if k.extent_is_unwritten(fs) {
        fs.event_inc(events::data_read_nopromote_unwritten);
        return fs.throw(Errcode::BCH_ERR_nopromote_unwritten);
    }

    if fs.target_congested(opts.promote_target) {
        fs.event_inc(events::data_read_nopromote_congested);
        return fs.throw(Errcode::BCH_ERR_nopromote_congested);
    }

    Ok(())
}

fn promote_new<'r>(t: &TransAttempt<'_, '_>, btree: Btree, k: BkeySC<'_>,
                   orig: &'r Rbio, opts: &c::bch_inode_opts,
                   failed: Option<&mut c::bch_io_failures>) -> Result<(&'r Rbio, AwaitingRead<'r>), BchError> {
    let fs = t.fs();

    if failed.is_none() {
        should_promote(fs, k, opts)?;
    }

    use c::WriteFlags as w;
    let mut update_opts = c::data_update_opts { write_flags: w::alloc_nowait, ..Default::default() };

    match failed.as_deref() {
        Some(f) if !f.is_empty() => {
            update_opts.type_ = c::DataUpdateType::self_heal;
            update_opts.target = opts.background_target;

            if let Some(kp) = k.arm::<Ptrs>() {
                for (i, p) in kp.ptrs(fs).enumerate() {
                    if f.dev(p.dev().into()).is_some() {
                        update_opts.ptrs_io_error |= 1 << i;
                        update_opts.ptrs_kill |= 1 << i;
                    }
                }
            }

            if update_opts.ptrs_kill == 0 {
                return fs.throw(Errcode::BCH_ERR_nopromote_no_rewrites);
            }
        }
        _ => {
            update_opts.type_ = c::DataUpdateType::promote;
            update_opts.target = opts.promote_target;
            update_opts.extra_replicas = 1;
            update_opts.write_flags |= w::cached | w::only_specified_devs;
        }
    }

    let in_flight = InFlight::tryget(fs)?;
    let start_time = local_clock();

    // SAFETY: the task borrows nothing, and promote_wq is drained before the
    // filesystem is freed
    let task = unsafe {
        update_task::spawn(t, Spawn::Queue(orig.fs().promote_wq()), Errcode::BCH_ERR_nopromote_enomem)
    }?;
    // On error - such as nocow_lock_blocked, ENOSPC_disk_reservation - the
    // box is dropped
    let mut u = task.init(t, None, None, c::write_point_specifier::hashed(current_id()), opts, update_opts, btree, k)?;

    let rbio = NonNull::from(&mut u.rbio);
    let task = u.awaiting_read(move |b| async move |_: &Update| run(b, in_flight, start_time).await);
    Ok((unsafe { rbio.as_ref() }, task))
}

/// A promote made: its read bio, the read's to read into as its bounce, and
/// its task, which the read starts with what it read when it's done with the
/// bio; and whether the read's to be of the whole extent - a self-heal, or
/// promote_whole_extents.
pub(crate) struct Promoted<'r> {
    pub rbio:  &'r Rbio,
    pub task:  AwaitingRead<'r>,
    pub whole: bool,
}

/// The promote of @k for read @orig, if there's to be one. With @failed, it's
/// the self-heal. A transaction restart, the one error it mustn't swallow, is
/// returned; anything else is no promote.
// Out of line: inlined, the update's setup - its opts, copied into the task -
// is in the frame of every read's fragment_of()
#[inline(never)]
pub(crate) fn promote_alloc<'r>(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, orig: &'r Rbio,
                                opts: &c::bch_inode_opts, failed: Option<&mut c::bch_io_failures>)
    -> Result<Option<Promoted<'r>>, BchError>
{
    let fs = t.fs();
    let self_healing = failed.is_some();
    let btree = if k.k.key_type() == BkeyType::reflink_v {
        Btree::reflink
    } else {
        Btree::extents
    };

    match promote_new(t, btree, k, orig, opts, failed) {
        Ok((rbio, task)) => {
            if self_healing {
                orig.self_healing.store(true, Relaxed);
            }
            Ok(Some(Promoted { rbio, task, whole: self_healing || fs.opts().promote_whole_extents.get() != 0 }))
        }
        // not swallowed: it'd leave the transaction poisoned, for a panic at
        // bch2_trans_put()
        Err(e) if is_restart(&e) => Err(e),
        Err(e) => {
            fs.event_inc_trace(events::data_read_nopromote, |out| {
                prt!(out, "{e}\n{}", k.to_text(fs));
            });
            Ok(None)
        }
    }
}
