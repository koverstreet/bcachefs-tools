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

use crate::alloc::{disk_groups::Target, write_point::WritePointSpecifier};
use crate::btree::{bkey::{BkeySC, BkeyV}, iter::TransAttempt};
use crate::data::update::{DataUpdateOpts, DataUpdateType};
use crate::opts::InodeOpts;
use crate::util::{time_stats::TimeStat, atomic::try_update, refcount::TryRefcount, task::Resume};
use crate::errcode::{Errcode, BchError};
use crate::data::extents::BkeyVExtents;
use crate::data::ptrs::IoFailures;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::read::Rbio;
use crate::data::update_task::{self, AwaitingRead, Body, ReadBody};
use crate::debug::{async_objs::AsyncObjSlot, trace::events};
use crate::fs::{Fs, WriteRef};
use crate::prt;
use crate::os::{kernel::{current_id, local_clock, num_online_cpus}, workqueue::Queue};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::in_place;
use crate::format::{Btree, BkeyType};

/// The in-flight limit, per online CPU.
const PROMOTES_PER_CPU: u32 = 32;

// ── The limit ────────────────────────────────────────────────────────────

impl Fs {
    pub fn promote_limit_tryget(&self) -> bool {
        let max = PROMOTES_PER_CPU * num_online_cpus();
        try_update(&self.promotes_in_flight, Relaxed, Relaxed, |n| (n < max).then_some(n + 1)).is_ok()
    }

    pub fn promote_limit_put(&self) {
        let n = self.promotes_in_flight.fetch_sub(1, Relaxed);
        assert!(n != 0, "promote limit put with no promote in flight");
    }

    fn promote_wq(&self) -> &Queue {
        &self.promote_wq
    }
}

/// What a promote holds while in flight - its update does, till it's dropped:
/// a write ref, so the filesystem doesn't go read-only under it, and its place
/// under the limit.
pub(crate) struct InFlight(WriteRef);

impl InFlight {
    fn tryget(fs: &Fs) -> Result<Self, BchError> {
        let write = fs.writes.tryget().ok_or_else(|| fs.err(Errcode::nopromote_no_writes))?;
        if !fs.promote_limit_tryget() {
            return fs.throw(Errcode::nopromote_ratelimited);
        }
        Ok(InFlight(write))
    }

    pub(crate) fn fs(&self) -> &Fs {
        &self.0
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.promote_limit_put();
    }
}

// ── The promote ──────────────────────────────────────────────────────────

/// The body, started when the read's done: the write.
#[in_place(PromoteFut)]
pub(crate) async fn run<'h>(mut u: Body<'h>, start_time: u64) {
    let fs = u.op.fs();
    let listed = unsafe { AsyncObjSlot::new(fs, &u.rbio) };

    let read = u.read();
    u.read_done.store(true, Relaxed);
    if read.ret.is_err() {
        drop(listed);
        return u.exit(Err(fs.err(Errcode::EIO_promote_read)));
    }
    {
        let k = BkeySC::from(u.index.k.as_ref());
        fs.event_add_trace(events::data_read_promote, k.k.size.into(), |out| prt!(out, "{}", k.to_text(fs)));
    }

    let _ = u.write(read, None, Resume::queue(fs.promote_wq())).await;
    fs.times.get(TimeStat::data_promote as usize).update(start_time);
    drop(listed);
    u.exit(Ok(()));
}

// ── Making one ───────────────────────────────────────────────────────────

fn should_promote(fs: &Fs, k: BkeySC<'_>, opts: &InodeOpts) -> Result<(), BchError> {
    assert!(opts.promote_target != 0, "should_promote() with no promote target");

    if k.arm::<Ptrs>().is_some_and(|k| k.has_target(fs, Target::decode(opts.promote_target.into()))) {
        fs.event_inc(events::data_read_nopromote_already_promoted);
        return fs.throw(Errcode::nopromote_already_promoted);
    }

    if k.extent_is_unwritten(fs) {
        fs.event_inc(events::data_read_nopromote_unwritten);
        return fs.throw(Errcode::nopromote_unwritten);
    }

    if fs.target_congested(opts.promote_target) {
        fs.event_inc(events::data_read_nopromote_congested);
        return fs.throw(Errcode::nopromote_congested);
    }

    Ok(())
}

/// A promote's body, once its read's done: run(), from when it began.
pub(crate) struct PromoteBody {
    start_time: u64,
}

impl ReadBody for PromoteBody {
    type Fut<'h> = PromoteFut<'h>;

    fn make(self, u: Body<'_>) -> PromoteFut<'_> {
        run(u, self.start_time)
    }
}

/// A promote's task, awaiting its read.
pub(crate) type PromoteTask = AwaitingRead<PromoteBody>;

fn promote_new<'r>(t: &TransAttempt<'_, '_>, fs: &'r Fs, btree: Btree, k: BkeySC<'_>, opts: &InodeOpts,
                   failed: Option<&mut IoFailures>) -> Result<PromoteTask, BchError> {

    if failed.is_none() {
        should_promote(fs, k, opts)?;
    }

    use crate::data::write::WriteFlags as w;
    let mut update_opts = DataUpdateOpts { write_flags: w::alloc_nowait, ..::pin_init::zeroed() };

    match failed.as_deref() {
        Some(f) if !f.is_empty() => {
            update_opts.type_ = DataUpdateType::self_heal;
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
                return fs.throw(Errcode::nopromote_no_rewrites);
            }
        }
        _ => {
            update_opts.type_ = DataUpdateType::promote;
            update_opts.target = opts.promote_target;
            update_opts.extra_replicas = 1;
            update_opts.write_flags |= w::cached | w::only_specified_devs;
        }
    }

    let in_flight = InFlight::tryget(fs)?;
    let start_time = local_clock();

    let task = update_task::spawn_held(t, in_flight, |fs| fs.promote_wq(), Errcode::nopromote_enomem)?;
    // On error - such as nocow_lock_blocked, ENOSPC_disk_reservation - the
    // box is dropped
    let u = task.init(t, None, None, WritePointSpecifier::hashed(current_id()), opts, update_opts, btree, k)?;

    Ok(u.awaiting_read(PromoteBody { start_time }))
}

/// A promote made: its task, whose read bio the read's to read into as its
/// bounce, and which the read starts with what it read when it's done with
/// the bio; and whether the read's to be of the whole extent - a self-heal,
/// or promote_whole_extents.
pub(crate) struct Promoted {
    pub task:  PromoteTask,
    pub whole: bool,
}

/// The promote of @k for read @orig, if there's to be one. With @failed, it's
/// the self-heal. A transaction restart, the one error it mustn't swallow, is
/// returned; anything else is no promote.
// Out of line: inlined, the update's setup - its opts, copied into the task -
// is in the frame of every read's fragment_of()
#[inline(never)]
pub(crate) fn promote_alloc<'r>(t: &TransAttempt<'_, '_>, fs: &'r Fs, k: BkeySC<'_>, orig: &'r Rbio,
                                opts: &InodeOpts, failed: Option<&mut IoFailures>)
    -> Result<Option<Promoted>, BchError>
{
    let self_healing = failed.is_some();
    let btree = if k.k.key_type() == BkeyType::reflink_v {
        Btree::reflink
    } else {
        Btree::extents
    };

    match promote_new(t, fs, btree, k, opts, failed) {
        Ok(task) => {
            if self_healing {
                orig.self_healing.store(true, Relaxed);
            }
            Ok(Some(Promoted { task, whole: self_healing || fs.opts().promote_whole_extents.get() != 0 }))
        }
        // not swallowed: it'd leave the transaction poisoned, for a panic at
        // the transaction's drop
        Err(e) if e.is_restart() => Err(e),
        Err(e) => {
            fs.event_inc_trace(events::data_read_nopromote, |out| {
                prt!(out, "{e}\n{}", k.to_text(fs));
            });
            Ok(None)
        }
    }
}
