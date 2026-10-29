// SPDX-License-Identifier: GPL-2.0

//! The sector allocator - foreground.c's bch2_alloc_sectors_req(): space for
//! a write, its copies' open buckets in its write point.
//!
//! A write's buckets come from bucket_set.rs's write_point_buckets(), and
//! when they don't make up its copies, it retries wider before it gives up:
//! without erasure coding, then on any device if its target isn't required,
//! then once more on its target's, where an allocation may now block. An
//! allocation that finds fewer devices than copies, or space copygc hasn't
//! freed yet, writes what it got - degraded, which is counted in the
//! superblock unless the write asked for best effort.
//!
//! Then the write point gets the buckets, aligned to the block size: the
//! space a write can take from it is what's free in all of them. One left
//! with none, alignment having taken what there was, goes back for a fresh
//! allocation. A failure gives the write point its buckets back - the ones
//! it had and the ones just got - and with too many write points in use,
//! frees one up and tries again.
//!
//! The write point's lock is held for the write: a LockedWritePoint, whose
//! append_ptrs() gives the write its pointers and whose drop - as
//! alloc_sectors_done() - keeps what's left of the buckets for the next
//! write. C's callers have it locked by bch2_alloc_sectors_req() and use
//! foreground.h's own append_ptrs and done, until they convert.

use crate::alloc::disk_groups::Target;
use crate::alloc::foreground::OpenBucket;
use crate::alloc::write_point::{WritePointGuard, WritePointSpec};
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::bch_errcode;
use crate::data::extents::bkey_append_ptr;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::opts::data_type_str;
use crate::sb::counters::events;
use crate::util::kernel::unlikely;
use core::ffi::c_int;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_export;

use c::bch_write_flags as w;

impl c::alloc_request {
    /// The devices it has copies on: its caller's, and the buckets it's got.
    fn devs_used(&self, fs: &Fs) -> c::DevsMask {
        let mut used = c::DevsMask::default();
        for &dev in self.devs_have().map_or(&[][..], |d| &d.data[..d.nr as usize]) {
            used.set(dev.into());
        }
        for ob in self.ptrs.iter(fs) {
            used.set(ob.id().dev().into());
        }
        used
    }

    /// Whether, with no target, it could go anywhere: unless it's to stay on
    /// its target's devices.
    fn may_widen(&self) -> bool {
        self.target != 0 && !self.flags.contains(w::BCH_WRITE_only_specified_devs)
    }
}

impl Fs {
    /// Buckets @req got beyond its copies, back to its write point, while
    /// the copies it keeps still make up its durability.
    fn deallocate_extra_replicas(&self, req: &mut c::alloc_request, wp: &mut c::WritePointLocked) {
        let mut extra = req.nr_effective - u32::from(req.nr_replicas);

        req.scratch_ptrs.nr = 0;
        for ob in req.ptrs.drain(self) {
            let d = u32::from(ob.ca().mi.get().durability);
            if d != 0 && d <= extra {
                extra -= d;
                wp.ptrs.push(ob);
            } else {
                req.scratch_ptrs.push(ob);
            }
        }
        req.ptrs = core::mem::take(&mut req.scratch_ptrs);
    }
}

impl<'f> TransAttempt<'_, 'f> {
    /// @req's buckets, its write point @wp locked: Ok(false) if alignment
    /// left it no space, and it's to be retried. @orig_target is the target
    /// its caller asked for, which a widened attempt clears from @req.
    fn alloc_sectors_wp(&self, req: &mut c::alloc_request, wp: &mut WritePointGuard<'_>, orig_target: u32)
        -> Result<bool, BchError>
    {
        let fs = self.fs();
        let matches = |e: Option<BchError>, class| e.is_some_and(|e| e.matches(class));

        self.relock()?;

        loop {
            req.devs_may_alloc = fs.target_rw_devs(req.data_type, Target::decode(req.target));
            // writes of cached data put their one copy on cache devices and
            // durable ones alike
            if req.flags.contains(w::BCH_WRITE_cached) {
                let cached = fs.target_rw_devs(c::bch_data_type::BCH_DATA_cached, Target::decode(req.target));
                req.devs_may_alloc.d.or(&cached.d);
            }

            // not the devices it has copies on, and away from their failure
            // domains
            let used = req.devs_used(fs);
            req.devs_may_alloc.d.andnot(&used);
            req.devs_chosen.d = used;
            req.devs_required = fs.dev_alloc_required(req).map_or(core::ptr::null(), |m| core::ptr::from_ref(m).cast());

            let mut err = self.write_point_buckets(req, wp).err();

            // before the retries below, which are gated on an error - and the
            // first drops the target: a targeted write would spill outside it
            // instead of replicating within it
            if matches(err, bch_errcode::BCH_ERR_ec_alloc_failed) {
                if req.flags.contains(w::BCH_WRITE_must_ec) {
                    return Err(err.expect("an error"));
                }
                req.bits.set_ec(false);
                req.bits.set_will_retry_target_devices(true);
                req.bits.set_will_retry_all_devices(req.may_widen());
                continue;
            }

            if let Some(e) = err.filter(|&e| !e.matches(bch_errcode::BCH_ERR_freelist_empty) &&
                                             !e.matches(bch_errcode::BCH_ERR_insufficient_devices)) {
                return Err(e);
            }

            if err.is_some() && req.bits.will_retry_all_devices() {
                // stripes only on the target asked for
                req.bits.set_ec(false);
                req.target = 0;
                req.bits.set_will_retry_all_devices(false);
                continue;
            }

            if err.is_some() && req.bits.will_retry_target_devices() {
                // across a target's devices, an allocation doesn't block till
                // each has been tried once
                req.bits.set_will_retry_target_devices(false);
                continue;
            }

            if req.nr_effective < u32::from(req.nr_replicas) && req.bits.ec() {
                if req.flags.contains(w::BCH_WRITE_must_ec) {
                    return fs.throw(bch_errcode::BCH_ERR_ec_alloc_failed_no_usable_block);
                }
                req.bits.set_ec(false);
                req.bits.set_will_retry_target_devices(true);
                req.bits.set_will_retry_all_devices(req.may_widen());
                continue;
            }

            if (matches(err, bch_errcode::BCH_ERR_insufficient_devices) ||
                matches(err, bch_errcode::BCH_ERR_bucket_alloc_no_progress)) && req.nr_effective != 0 {
                // fewer copies than asked for, succeeding: counted, as nothing
                // else counts it. insufficient_devices is what a reservation
                // should have refused; no_progress is space copygc hasn't
                // compacted yet
                if req.nr_effective < u32::from(req.nr_replicas) &&
                   !req.flags.contains(w::BCH_WRITE_replicas_best_effort) {
                    fs.sb_error_count(match matches(err, bch_errcode::BCH_ERR_insufficient_devices) {
                        true  => c::bch_sb_error_id::BCH_FSCK_ERR_write_degraded_insufficient_devices,
                        false => c::bch_sb_error_id::BCH_FSCK_ERR_write_degraded_no_progress,
                    });
                }
                err = None;
            }

            // an allocation blocks only with no retry left: one that didn't -
            // its last try succeeding - and is short of copies goes again, to
            // wait
            if matches(err, bch_errcode::BCH_ERR_freelist_empty) &&
               !matches(err, bch_errcode::BCH_ERR_bucket_alloc_no_progress) &&
               !req.cl.is_null() && !req.flags.contains(w::BCH_WRITE_alloc_nowait) {
                continue;
            }

            if let Some(e) = err {
                return Err(e);
            }
            assert!(req.nr_effective != 0 || req.flags.contains(w::BCH_WRITE_cached),
                    "a write allocated nothing");
            break;
        }

        if req.bits.ec() && req.flags.contains(w::BCH_WRITE_must_ec) &&
           !req.ptrs.iter(fs).any(|ob| !ob.ec.load(Relaxed).is_null()) {
            return fs.throw(bch_errcode::BCH_ERR_ec_alloc_failed_no_ec_bucket);
        }

        // a target with durability 0 devices: a cached copy on one too - not
        // for moves, which move only the data there is, nor for writes of
        // cached data, which are on the cache devices already
        if !req.flags.intersects(w::BCH_WRITE_move | w::BCH_WRITE_cached) &&
           req.data_type == c::bch_data_type::BCH_DATA_user {
            // from the target asked for: devs_may_alloc's been narrowed by
            // each copy, and may have been widened
            req.devs_may_alloc = fs.target_rw_devs(c::bch_data_type::BCH_DATA_cached, Target::decode(orig_target));
            let used = req.devs_used(fs);
            req.devs_may_alloc.d.andnot(&used);

            if req.devs_may_alloc.d.weight() != 0 {
                self.bucket_alloc_cached(req, wp)?;
            }
        }

        if req.nr_effective > u32::from(req.nr_replicas) {
            fs.deallocate_extra_replicas(req, wp);
        }

        // the buckets it had, not used
        wp.ptrs.drain(fs).for_each(|ob| fs.open_bucket_free_unused(ob));
        wp.ptrs = core::mem::take(&mut req.ptrs);

        // aligned to the block size - misaligned bucket sizes, from old
        // tools, or writes mixing logical and physical alignment
        let block_sectors = (fs.block_bytes() >> 9) as u32;
        wp.sectors_free = wp.ptrs.iter(fs).map(|ob| {
            let ca = ob.ca();
            let free = ob.sectors_free.get();
            let offset = ca.bucket_to_sector(ob.id().bucket()) + u64::from(ca.mi.get().bucket_size) - u64::from(free);
            let align = (offset.next_multiple_of(block_sectors.into()) - offset) as u32;

            let free = free.saturating_sub(align);
            ob.sectors_free.set(free);
            free
        }).min().unwrap_or(u32::MAX);
        wp.prev_sectors_free = wp.sectors_free;
        wp.sectors_free -= wp.sectors_free % block_sectors;

        if unlikely(wp.sectors_free == 0) {
            return Ok(false);
        }
        assert!(wp.sectors_free != u32::MAX, "a write point with no buckets");
        Ok(true)
    }

    /// Space for a write of @req's, from the write point @spec names: that,
    /// locked, its buckets @req's copies, as much as all of them have free
    /// in its sectors_free.
    pub(crate) fn alloc_sectors_req(&self, req: &mut c::alloc_request, spec: WritePointSpec<'f>)
        -> Result<(&'f c::write_point, WritePointGuard<'f>), BchError>
    {
        let fs = self.fs();
        let a = &fs.allocator;
        let orig_target = req.target;
        assert!(req.nr_replicas != 0, "allocating for no copies");

        loop {
            req.ca = core::ptr::null_mut();
            req.wake_all_counter_snapshot = a.wake_all_counter.load(Relaxed) as u32;
            req.bits.set_will_retry_all_devices(req.may_widen());
            req.bits.set_will_retry_target_devices(!req.flags.contains(w::BCH_WRITE_alloc_nowait));
            req.bits.set_copygc_can_make_progress(false);
            req.bits.set_trace_alloc_failed(false);
            req.ptrs.nr = 0;
            req.nr_effective = 0;
            req.trace_reset();
            let write_points_nr = a.write_points_hash_lock.write_points_nr.get();

            let (wp, mut g) = fs.writepoint_find(self, spec);
            req.wp = core::ptr::from_ref(wp).cast_mut();
            req.data_type = wp.alloc.data_type;

            match self.alloc_sectors_wp(req, &mut g, orig_target) {
                Ok(true) => return Ok((wp, g)),
                Ok(false) => fs.alloc_sectors_done(wp, g),
                Err(e) => {
                    // the write point keeps them all: its own, and those just got
                    for ob in g.ptrs.drain(fs) {
                        if (req.ptrs.nr as usize) < req.ptrs.v.len() {
                            req.ptrs.push(ob);
                        } else {
                            fs.open_bucket_free_unused(ob);
                        }
                    }
                    g.ptrs = core::mem::take(&mut req.ptrs);
                    drop(g);

                    let blocked = e.matches(bch_errcode::BCH_ERR_bucket_alloc_blocked);
                    if !(blocked || e.matches(bch_errcode::BCH_ERR_freelist_empty)) ||
                       !fs.try_decrease_writepoints(self, write_points_nr) {
                        return Err(e);
                    }
                    if blocked {
                        fs.alloc_waiters_unpark();
                    }
                }
            }
        }
    }
}

// ── Writing to a write point ─────────────────────────────────────────────

impl Fs {
    /// A write done with @wp, @g its lock: its buckets with less than a
    /// block left put - after the unlock - the rest kept for the next write,
    /// and the sectors the write took counted.
    pub(crate) fn alloc_sectors_done(&self, wp: &c::write_point, mut g: WritePointGuard<'_>) {
        let block_sectors = (self.block_bytes() >> 9) as u32;
        let (mut keep, mut spent) = (c::open_buckets::default(), c::open_buckets::default());
        for ob in g.ptrs.drain(self) {
            match ob.sectors_free.get() < block_sectors {
                true  => spent.push(ob),
                false => keep.push(ob),
            }
        }
        g.ptrs = keep;

        self.event_add_trace(events::sectors_alloc, g.prev_sectors_free.wrapping_sub(g.sectors_free).into(),
                             |out| write!(out, "{}", data_type_str(wp.alloc.data_type)));
        drop(g);
        spent.drain(self).for_each(drop);
    }
}

/// A write point with room for a write, locked until it's dropped: the
/// write's pointers appended to its key from it, and dropped,
/// alloc_sectors_done().
pub struct LockedWritePoint<'f> {
    fs: &'f Fs,
    wp: &'f c::write_point,
    /* taken by the drop */
    g:  Option<WritePointGuard<'f>>,
}

impl<'f> TransAttempt<'_, 'f> {
    /// @write_point, locked, with buckets allocated for @req.
    pub fn alloc_sectors(&self, req: &mut c::alloc_request, write_point: c::write_point_specifier)
        -> Result<LockedWritePoint<'f>, BchError>
    {
        // SAFETY: a write's specifier is writepoint_hashed()'s, or
        // writepoint_ptr()'s of one of the filesystem's write points
        let spec = unsafe { write_point.spec(self.fs()) };
        let (wp, g) = self.alloc_sectors_req(req, spec)?;
        Ok(LockedWritePoint { fs: self.fs(), wp, g: Some(g) })
    }
}

impl LockedWritePoint<'_> {
    fn guard(&self) -> &WritePointGuard<'_> {
        self.g.as_ref().expect("locked till it's dropped")
    }

    /// What its buckets have room for, for this write.
    pub fn sectors_free(&self) -> u32 {
        self.guard().sectors_free
    }

    /// Takes a pin on each of its buckets, into @ptrs, for the data's writer
    /// to hold until it's indexed.
    pub fn open_buckets_get(&self, ptrs: &mut c::open_buckets) {
        for ob in self.guard().ptrs.iter(self.fs) {
            ob.data_type.set(self.wp.alloc.data_type.0 as u8);
            ptrs.push(OpenBucket::get(ob));
        }
    }

    /// Appends to @k a pointer to the next @sectors of each of its buckets -
    /// cached, on a device of durability 0 for user data - and takes them
    /// from what's free.
    ///
    /// # Safety
    /// @k is a key with pointers - btree_ptr, btree_ptr_v2, extent - with
    /// room after its value for one per bucket, on none of their devices.
    pub unsafe fn append_ptrs(&mut self, k: *mut c::bkey_i, sectors: u32) {
        let (fs, user) = (self.fs, self.wp.alloc.data_type == c::bch_data_type::BCH_DATA_user);
        let g = self.g.as_mut().expect("locked till it's dropped");
        assert!(sectors <= g.sectors_free, "{sectors} sectors from a write point with {} free", g.sectors_free);
        g.sectors_free -= sectors;
        g.sectors_allocated += u64::from(sectors);

        for ob in g.ptrs.iter(fs) {
            let ptr = ob.ptr().with_cached(u8::from(user && ob.ca().mi.get().durability == 0));
            // SAFETY: the caller's
            unsafe { bkey_append_ptr(k, ptr) };

            let free = ob.sectors_free.get();
            assert!(sectors <= free, "{sectors} sectors from open bucket {} with {free} free", ob.idx().get());
            ob.sectors_free.set(free - sectors);
        }
    }
}

impl Drop for LockedWritePoint<'_> {
    fn drop(&mut self) {
        let g = self.g.take().expect("locked till it's dropped");
        self.fs.alloc_sectors_done(self.wp, g);
    }
}

// ── For C ────────────────────────────────────────────────────────────────

/// For C: alloc_sectors_req(), its write point in @wp_ret - locked, and left
/// so for bch2_alloc_sectors_done(). On an error too, unlocked: an allocation
/// that blocked leaves __bch2_write() queueing its op on the write point.
///
/// # Safety
/// @trans is in an attempt; @write_point is hashed, or a writepoint_ptr() of
/// one of the filesystem's write points.
#[c_export]
pub unsafe fn bch2_alloc_sectors_req(trans: &BtreeTrans<'_>, req: &mut c::alloc_request,
                                     write_point: c::write_point_specifier,
                                     wp_ret: &mut *mut c::write_point) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    let spec = unsafe { write_point.spec(t.fs()) };
    let ret = t.alloc_sectors_req(req, spec).map(|(_, g)| g.leave_locked());
    // the write point last found, whatever came of allocating from it
    *wp_ret = req.wp;
    ret_to_c(ret)
}
