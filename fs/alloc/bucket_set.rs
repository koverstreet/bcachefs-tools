// SPDX-License-Identifier: GPL-2.0

//! A request's buckets, as a set - foreground.c's: one per copy, added to
//! req.ptrs until the request's durability is met.
//!
//! A write's are taken, in order, from what costs least: its write point's
//! own buckets, still open from its last write; the partial list - buckets
//! a write point gave up with space left - the domain-best first; then
//! fresh ones, from a stripe being made if it's erasure coded, else from
//! the devices in turn, as dev_select.rs orders them, re-sorted after each
//! copy as it changes which failure domains the next should avoid.
//!
//! What's under the write point's lock - its buckets and its round robin -
//! is the WritePointLocked the caller holds the lock for: the sector
//! allocator, which takes it to allocate. The write point's data type is
//! req.data_type, which the sector allocator sets from it.
//!
//! A request's done when its durability's met - or at its first bucket, for
//! a cached write, a stripe's block, or a cache device's: add_new_bucket().

use crate::alloc::foreground::{FreelistGuard, OpenBucket, OpenBucketRef};
use crate::btree::iter::{is_restart, TransAttempt};
use crate::c;
use crate::c::bch_errcode;
use crate::data::ec::head::{head_get, stripe_head_put};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::sb::counters::events;
use crate::util::ToText;
use arrayvec::ArrayVec;
use core::sync::atomic::Ordering::Relaxed;

/// Partial buckets taken while placement was restricted - their devices,
/// and the copies the request had before each - traced once freelist_lock
/// is dropped. Placement restricts only 2 to BCH_REPLICAS_MAX copies, so
/// that's as many as there can be.
type Restricted = ArrayVec<(u32, u32), { c::BCH_REPLICAS_MAX as usize }>;

impl c::alloc_request {
    /// @ob, added: its device chosen, its durability counted, its pin in
    /// ptrs. Whether that's the request done: its durability met, or the one
    /// copy of a cached write, a stripe's block, or a cache device's bucket.
    fn add_new_bucket(&mut self, ob: OpenBucket) -> bool {
        let durability = u32::from(ob.ca().mi.get().durability);
        assert!(self.nr_effective < u32::from(self.nr_replicas) || durability == 0,
                "a bucket for a request already complete");

        let dev = usize::from(ob.id().dev());
        let ec = !ob.ec.load(Relaxed).is_null();
        self.devs_may_alloc.d.clear(dev);
        self.devs_chosen.d.set(dev);
        self.nr_effective += durability;
        self.ptrs.push(ob);

        self.nr_effective >= u32::from(self.nr_replicas) ||
            self.flags.contains(c::bch_write_flags::BCH_WRITE_cached) || ec || durability == 0
    }

    /// Whether the open bucket @ob - a write point's, or the partial list's
    /// - will do for it: on a device it may use, of its data type, erasure
    /// coded as it is.
    fn want_bucket(&self, ob: &OpenBucketRef) -> bool {
        self.devs_may_alloc.d.test(ob.id().dev().into()) &&
            ob.data_type.get() == self.data_type.0 as u8 &&
            self.bits.ec() == !ob.ec.load(Relaxed).is_null()
    }
}

impl Fs {
    /// From @wp's buckets, those @req wants - until it's done - the rest
    /// left there.
    fn bucket_alloc_set_writepoint(&self, req: &mut c::alloc_request, wp: &mut c::WritePointLocked) -> bool {
        let mut done = false;

        req.scratch_ptrs.nr = 0;
        for ob in wp.ptrs.drain(self) {
            if !done && req.want_bucket(&ob) {
                done = req.add_new_bucket(ob);
            } else {
                req.scratch_ptrs.push(ob);
            }
        }
        wp.ptrs = req.scratch_ptrs;
        done
    }

    /// The partial list's @i'th, freelist_lock held, added to @req - into
    /// @restricted, if placement's restricted, for its trace.
    fn partial_bucket_alloc(&self, g: &mut FreelistGuard<'_>, req: &mut c::alloc_request, i: usize,
                            restricted: &mut Restricted) -> bool {
        let ob = self.open_buckets_partial_take(g, i);
        if req.devs_required().is_some() {
            let pick = (ob.id().dev().into(), req.nr_effective);
            if restricted.try_push(pick).is_err() {
                // more than there can be: counted, untraced
                self.event_inc(events::bucket_alloc_placement_restricted);
            }
        }
        req.add_new_bucket(ob)
    }

    /// @restricted traced, freelist_lock dropped.
    fn partial_restricted_trace(&self, req: &c::alloc_request, restricted: &Restricted) {
        for &(dev, have) in restricted {
            self.placement_restricted_count(req, dev, have);
        }
    }

    /// From the partial list, the one in the failure domain best for @req
    /// each round - not the first that will do: most allocations end here,
    /// and first fit would undo bucket_alloc_set()'s spreading of fresh
    /// buckets - until it's done, or none will do.
    fn bucket_alloc_set_partial(&self, req: &mut c::alloc_request) -> bool {
        let a = &self.allocator;
        if a.freelist_lock.open_buckets_partial_nr.get() == 0 {
            return false;
        }

        let mut restricted = Restricted::new();
        let done = self.bucket_alloc_set_partial_locked(&mut a.freelist_lock.lock(), req, &mut restricted);
        self.partial_restricted_trace(req, &restricted);
        done
    }

    fn bucket_alloc_set_partial_locked(&self, g: &mut FreelistGuard<'_>, req: &mut c::alloc_request,
                                       restricted: &mut Restricted) -> bool {
        let a = &self.allocator;
        while a.freelist_lock.open_buckets_partial_nr.get() != 0 {
            // each pick changes devs_chosen, and so the keys
            self.domain_keys_update(req);

            // from the end, as C's: of equal keys, the last
            let mut best = None;
            let mut best_key = u64::MAX;
            for (i, ob) in a.partial(g).enumerate().rev() {
                if !req.want_bucket(ob) || !req.required_leaves_room(ob.id().dev().into()) {
                    continue;
                }

                let ca = ob.ca();
                req.usage = ca.usage_read_fast();
                if ca.free_buckets_at(&req.usage, req.watermark) + u64::from(ca.nr_partial_buckets.get()) == 0 {
                    continue;
                }

                let key = req.domain_keys[usize::from(ob.id().dev())];
                if key < best_key {
                    best = Some(i);
                    best_key = key;
                }
            }

            let Some(best) = best else { return false };
            if self.partial_bucket_alloc(g, req, best, restricted) {
                return true;
            }
        }
        false
    }
}

impl TransAttempt<'_, '_> {
    /// Fresh buckets for @req, a device at a time in @stripe's order, re-sorted
    /// after each copy, until it's done - their pins in req.ptrs, as are those
    /// it got before an error. Running out of devices, the last one's error,
    /// or insufficient_devices, traced. A restart, a blocked allocation or no
    /// open buckets end it at once: no other device will do better.
    pub fn bucket_alloc_set(&self, req: &mut c::alloc_request, stripe: &mut c::dev_stripe_state)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        assert!(req.nr_effective < u32::from(req.nr_replicas), "allocating for a request already complete");

        fs.dev_alloc_list(stripe, req);
        // the whole allocation's, for tracing and bailing: not to flip as the
        // list shrinks
        if req.devs_sorted.nr <= 1 {
            req.bits.set_will_retry_target_devices(false);
        }

        let mut ret = Ok(());
        loop {
            let mut progress = false;
            let nr = req.devs_sorted.nr as usize;

            for i in 0..nr {
                let Some(ca) = fs.dev_get(req.devs_sorted.data[i].into()) else { continue };
                req.bits.set_will_retry_set_devices(i + 1 < nr);

                match self.bucket_alloc(req, &ca) {
                    Ok(ob) => {
                        stripe.increment(&ca, &req.usage);
                        fs.placement_restricted_count(req, ob.id().dev().into(), req.nr_effective);
                        if req.add_new_bucket(ob) {
                            return Ok(());
                        }
                        progress = true;
                        break;
                    }
                    Err(e) if is_restart(&e) ||
                              e.matches(bch_errcode::BCH_ERR_operation_blocked) ||
                              e.matches(bch_errcode::BCH_ERR_open_buckets_empty) => return Err(e),
                    Err(e) => ret = Err(e),
                }
            }

            if !progress {
                break;
            }
            fs.dev_alloc_list(stripe, req);
        }

        ret?;
        let e = fs.err(bch_errcode::BCH_ERR_insufficient_devices);
        req.trace_add(c::BCH_SB_MEMBER_INVALID as u8, Some(e), 0, 0, false);
        Err(e)
    }

    /// For an erasure coded write: a block of the stripe @req's stripe head
    /// is making, if it has one for us - placed by @wp's round robin.
    fn bucket_alloc_from_stripe(&self, req: &mut c::alloc_request, wp: &mut c::WritePointLocked)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let (_, g, ob) = head_get(self, req, 0, Some(&mut wp.stripe))?;

        if let Some(ob) = ob {
            fs.event_inc_trace(events::bucket_alloc_from_stripe, |out| ob.to_text(out));
            req.add_new_bucket(ob);
        }
        stripe_head_put(fs, g);
        Ok(())
    }

    /// A write's buckets: @wp's own, the partial list's, then fresh ones -
    /// a stripe's, if it's erasure coded.
    pub(crate) fn write_point_buckets(&self, req: &mut c::alloc_request, wp: &mut c::WritePointLocked)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        if fs.bucket_alloc_set_writepoint(req, wp) || fs.bucket_alloc_set_partial(req) {
            return Ok(());
        }
        match req.bits.ec() {
            true  => self.bucket_alloc_from_stripe(req, wp),
            false => self.bucket_alloc_set(req, &mut wp.stripe),
        }
    }

    /// One more copy, a cached one, for a write already complete: from @wp,
    /// the partial list, or fresh - never blocking, as a write doesn't wait
    /// for its cached copy, and not getting one isn't an error. Only a
    /// restart is.
    pub(crate) fn bucket_alloc_cached(&self, req: &mut c::alloc_request, wp: &mut c::WritePointLocked)
        -> Result<(), BchError>
    {
        let fs = self.fs();

        let mine = wp.ptrs.iter(fs).position(|ob| req.devs_may_alloc.d.test(ob.id().dev().into()));
        if let Some(i) = mine {
            req.add_new_bucket(wp.ptrs.remove(fs, i));
            return Ok(());
        }

        let a = &fs.allocator;
        if a.freelist_lock.open_buckets_partial_nr.get() != 0 {
            let mut restricted = Restricted::new();
            let found = {
                let mut g = a.freelist_lock.lock();
                let i = a.partial(&g).position(|ob| ob.data_type.get() == req.data_type.0 as u8 &&
                                                    req.devs_may_alloc.d.test(ob.id().dev().into()));
                if let Some(i) = i {
                    fs.partial_bucket_alloc(&mut g, req, i, &mut restricted);
                }
                i.is_some()
            };
            fs.partial_restricted_trace(req, &restricted);
            if found {
                return Ok(());
            }
        }

        let (nr_replicas, cl) = (req.nr_replicas, req.cl);
        req.nr_replicas = (req.nr_effective + 1) as u8;
        req.cl = core::ptr::null_mut();
        // a cached copy spends no durable space
        req.devs_required = core::ptr::null();

        let ret = self.bucket_alloc_set(req, &mut wp.stripe);

        req.nr_replicas = nr_replicas;
        req.cl = cl;
        match ret {
            Err(e) if is_restart(&e) => Err(e),
            _ => Ok(()),
        }
    }
}
