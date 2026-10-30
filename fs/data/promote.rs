// SPDX-License-Identifier: GPL-2.0

//! Promotes: when a read's data should also be on the promote target - a
//! cache - a data update writes a cached copy there. The self-heal is the
//! same for a read with IO errors: it rewrites the replicas that failed.
//! Promotes are opportunistic, so they're skipped when they can't be done
//! cheaply: when the target is congested, or too many are in flight.
//!
//! A promote is an UpdateTask (update_task.rs) on promote_wq.
//! bch2_promote_alloc(), called partway through a read, decides whether to
//! promote, and if so makes the update and returns its read bio: a bounce
//! split of the read, for the read path to read into. The read path signals
//! the read done when it frees the split, by bch2_promote_read_done().
//!
//! The in-flight limit is one count for the filesystem, PROMOTES_PER_CPU per
//! online CPU.

use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{BkeySC, BkeyV};
use crate::btree::iter::{is_restart, BtreeTrans, TransAttempt};
use crate::c;
use crate::c::bch_errcode;
use crate::data::extents::BkeyVExtents;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::update::bch2_read_bio_to_text;
use crate::data::update_task::{Body, Update, UpdateTask};
use crate::errcode::{err_ptr, BchError};
use crate::fs::{writepoint_hashed, Fs, WriteRef};
use crate::prt;
use crate::sb::counters::events;
use crate::util::backref::BackRef;
use crate::util::kernel::{current_id, local_clock, num_online_cpus};
use crate::util::refcount::TryRefcount;
use crate::util::time_stats::TimeStats;
use crate::util::task::Spawn;
use crate::util::workqueue::Queue;
use crate::util::{Printbuf, ToText};
use core::ffi::c_int;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_export;

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
        // SAFETY: the filesystem's, for as long as it is
        unsafe { Queue::from_raw(self.promote_wq) }
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
        let write = fs.writes.tryget_live().ok_or_else(|| fs.err(bch_errcode::BCH_ERR_nopromote_no_writes))?;
        if !fs.promote_limit_tryget() {
            return fs.throw(bch_errcode::BCH_ERR_nopromote_ratelimited);
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

/// A promote, as the data_update list shows it: the parent read too, till its
/// read's done.
pub(crate) fn promote_to_text(u: &c::data_update, out: &mut Printbuf, fs: &Fs) {
    if !u.read_done.load(Relaxed) {
        // SAFETY: a split's parent, valid until the split is done
        let parent = unsafe { u.rbio.completion.parent };
        prt!(out, "parent read: {parent:p}\n");
        let mut out = out.indent(2);
        // SAFETY: read only
        unsafe { bch2_read_bio_to_text(out.as_raw(), fs.as_ptr(), parent) };
    }
    u.to_text(out);
}

/// The body: the read, then the write. @in_flight is held until after exit().
async fn run(mut u: Body, in_flight: InFlight, start_time: u64) {
    let fs = u.op.fs();

    u.read().await;
    if u.rbio.ret != 0 {
        u.rbio.async_list_del(fs);
        return u.exit(-(c::EIO as c_int));
    }
    {
        let k = BkeySC::from(u.k.key());
        fs.event_add_trace(events::data_read_promote, k.k.size.into(), |out| prt!(out, "{}", k.to_text(fs)));
    }

    u.write().await;
    fs.times[c::bch_time_stats::BCH_TIME_data_promote as usize].update(start_time);
    u.rbio.async_list_del(fs);
    u.exit(0);
    drop(in_flight);
}

/// Called by rbio_free() when the read path is done with the promote's split.
///
/// # Safety
/// @rbio is a promote's, from bch2_promote_alloc().
#[c_export]
pub unsafe fn bch2_promote_read_done(rbio: *mut c::bch_read_bio) {
    // SAFETY: the caller's
    unsafe { Update::read_done(rbio.byte_sub(core::mem::offset_of!(c::data_update, rbio)).cast()) }
}

// ── Making one ───────────────────────────────────────────────────────────

fn should_promote(fs: &Fs, k: BkeySC<'_>, opts: &c::bch_inode_opts) -> Result<(), BchError> {
    assert!(opts.promote_target != 0, "should_promote() with no promote target");

    if k.arm::<Ptrs>().is_some_and(|k| k.has_target(fs, Target::decode(opts.promote_target.into()))) {
        fs.event_inc(events::data_read_nopromote_already_promoted);
        return fs.throw(bch_errcode::BCH_ERR_nopromote_already_promoted);
    }

    if k.extent_is_unwritten(fs) {
        fs.event_inc(events::data_read_nopromote_unwritten);
        return fs.throw(bch_errcode::BCH_ERR_nopromote_unwritten);
    }

    if fs.target_congested(opts.promote_target) {
        fs.event_inc(events::data_read_nopromote_congested);
        return fs.throw(bch_errcode::BCH_ERR_nopromote_congested);
    }

    Ok(())
}

fn promote_new(t: &TransAttempt<'_, '_>, btree: c::btree_id, k: BkeySC<'_>,
               orig: &mut c::bch_read_bio, opts: &c::bch_inode_opts,
                      failed: Option<&mut c::bch_io_failures>) -> Result<NonNull<c::bch_read_bio>, BchError> {
    let fs = t.fs();

    if failed.is_none() {
        should_promote(fs, k, opts)?;
    }

    use c::bch_write_flags as w;
    let mut update_opts = c::data_update_opts { write_flags: w::BCH_WRITE_alloc_nowait, ..Default::default() };

    match failed.as_deref() {
        Some(f) if f.nr != 0 => {
            update_opts.type_ = c::bch_data_update_types::BCH_DATA_UPDATE_self_heal;
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
                return fs.throw(bch_errcode::BCH_ERR_nopromote_no_rewrites);
            }
        }
        _ => {
            update_opts.type_ = c::bch_data_update_types::BCH_DATA_UPDATE_promote;
            update_opts.target = opts.promote_target;
            update_opts.extra_replicas = 1;
            update_opts.write_flags |= w::BCH_WRITE_cached | w::BCH_WRITE_only_specified_devs;
        }
    }

    let in_flight = InFlight::tryget(fs)?;
    let start_time = local_clock();

    // SAFETY: the task borrows nothing, and promote_wq is drained before the
    // filesystem is freed
    let task = unsafe {
        UpdateTask::spawn(t, Spawn::Queue(fs.promote_wq()), bch_errcode::BCH_ERR_nopromote_enomem)
    }?;
    // On error - such as nocow_lock_blocked, ENOSPC_disk_reservation - the
    // task goes unstarted
    let mut u = task.init(t, None, None, writepoint_hashed(current_id()), opts, update_opts, btree, k)?;
    let failed = failed.map_or(core::ptr::null_mut(), core::ptr::from_mut);
    u.rbio.init_fragment(orig, failed);
    u.rbio.bits_mut().set_bounce(1);
    u.rbio.bits_mut().set_promote(1);

    let rbio = NonNull::from(&mut u.rbio);
    u.start(move |b| run(b, in_flight, start_time));
    Ok(rbio)
}

/// Returns the bounce split for __bch2_read_extent() to read into, or null
/// for no promote. A transaction restart, the one error it mustn't swallow,
/// is returned as an ERR_PTR(). With @failed, it's the self-heal. @read_full
/// is set if the read must cover the whole extent, as a promote's update
/// always reads it.
///
/// # Safety
/// Called partway through an attempt on @trans. @orig is the read, which
/// outlives the promote's.
#[c_export]
pub unsafe fn bch2_promote_alloc(trans: &BtreeTrans<'_>, k: BkeySC<'_>, orig: &mut c::bch_read_bio,
                                 opts: &c::bch_inode_opts, bounce: &mut bool, read_full: &mut bool,
                                 failed: Option<&mut c::bch_io_failures>) -> *mut c::bch_read_bio {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    let fs = t.fs();
    let self_healing = failed.is_some();

    // the retry path, before anything is known to need repair: no promote
    if failed.as_deref().is_some_and(|f| f.nr == 0) {
        return core::ptr::null_mut();
    }

    // already a data update: IO errors go back to it
    if self_healing && orig.bits().data_update() != 0 {
        return core::ptr::null_mut();
    }

    let full = self_healing || *read_full || fs.opts().promote_whole_extents.get() != 0;

    let btree = if k.k.key_type() == c::bch_bkey_type::KEY_TYPE_reflink_v {
        c::btree_id::reflink
    } else {
        c::btree_id::extents
    };

    match promote_new(&t, btree, k, orig, opts, failed) {
        Ok(rbio) => {
            *bounce = true;
            *read_full = full;
            if self_healing {
                orig.bits_mut().set_self_healing(1);
            }
            rbio.as_ptr()
        }
        // not swallowed: it'd leave the transaction poisoned, for a panic at
        // bch2_trans_put()
        Err(e) if is_restart(&e) => err_ptr(e),
        Err(e) => {
            fs.event_inc_trace(events::data_read_nopromote, |out| {
                prt!(out, "{e}\n{}", k.to_text(fs));
            });
            core::ptr::null_mut()
        }
    }
}
