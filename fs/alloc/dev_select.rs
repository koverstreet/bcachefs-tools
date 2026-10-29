// SPDX-License-Identifier: GPL-2.0

//! Device selection for allocation - foreground.c's: which devices a write's
//! copies go to, in what order.
//!
//! A dev_stripe_state is a round robin over the devices - a write point's,
//! the journal's, a stripe head's: a clock hand per device, the smallest
//! allocated from, each allocation adding the inverse of the device's free
//! space, so the round robin favours free space. Hands are rescaled before
//! they can overflow; a device joining the set starts at the least hand of
//! the rest, not at 0, where it would win every comparison till it caught up.
//!
//! The candidates' order puts failure domains first - devices whose domain
//! holds the fewest copies already chosen, so copies spread across domains;
//! for erasure coding, a device sharing a chosen block's domain is excluded -
//! then the devices placement requires (capacity.rs's Placement: free space
//! lopsided enough that a device only fills if every write puts a copy on
//! it), then the clock hands.

use crate::c;
use crate::fs::Fs;
use crate::sb::counters::events;
use crate::util::ffi::Opaque;
use crate::util::kernel::likely;
use crate::util::projections::Volatile;
use crate::util::rcu::read_lock;
use cstruct_macros::c_export;

const NR_DEVS: usize = c::BCH_SB_MEMBERS_MAX as usize;

const HAND_RESCALE: u64 = 1 << 62;    /* rescale past this */
const HAND_MAX: u64     = 1 << 56;    /* the most, after a rescale */
const HAND_INV: u64     = 1 << 52;    /* the most an allocation adds: no free space */

impl c::dev_stripe_state {
    /// @devs the devices it's choosing between: those new to it join at the
    /// least hand of the rest - raised to it, never lowered, so one rejoining
    /// keeps its history, and doesn't win every tie while its siblings starve.
    fn sync(&mut self, devs: &c::bch_devs_mask) {
        if likely(self.cached_devs.d == devs.d) {
            return;
        }

        let mut added = devs.d;
        added.andnot(&self.cached_devs.d);
        if let Some(min) = devs.d.iter().filter(|&i| !added.test(i)).map(|i| self.next_alloc[i]).min() {
            for i in added.iter() {
                self.next_alloc[i] = self.next_alloc[i].max(min);
            }
        }
        self.cached_devs = *devs;
    }

    /// The hands brought down before they overflow: by as much as takes the
    /// lowest nonzero to 0, or as takes the highest to HAND_MAX, whichever's
    /// more. Hands of devices rarely tried - a target's fallback - can be far
    /// apart, and one at 0 has lost its history.
    #[cold]
    fn rescale(&mut self) {
        let scale_max = self.next_alloc.iter().copied().filter(|&v| v != 0).min().unwrap_or(u64::MAX);
        let scale_min = self.next_alloc.iter().map(|&v| v.saturating_sub(HAND_MAX)).max().unwrap_or(0);
        let scale = scale_min.max(scale_max);

        for v in &mut self.next_alloc {
            *v = v.saturating_sub(scale);
        }
    }

    /// An allocation from @ca, its usage @usage: its hand advanced by the
    /// inverse of its free space.
    pub fn increment(&mut self, ca: &c::bch_dev, usage: &c::bch_dev_usage) {
        let free = ca.free_buckets_at(usage, c::bch_watermark::BCH_WATERMARK_normal);
        let v = &mut self.next_alloc[usize::from(ca.dev_idx)];

        *v = v.saturating_add(if free != 0 { HAND_INV / free } else { HAND_INV });
        if *v > HAND_RESCALE {
            self.rescale();
        }
    }

    /// @devs into @ret, ordered for allocation: by @domain_keys, then those
    /// @required, then the hands - ties as they came, by device index.
    fn alloc_list(&mut self, devs: &c::bch_devs_mask, domain_keys: &[u64; NR_DEVS],
                  required: Option<&c::DevsMask>, ret: &mut c::dev_alloc_list) {
        self.sync(devs);

        ret.nr = 0;
        for i in devs.d.iter() {
            ret.data[ret.nr as usize] = i as u8;
            ret.nr += 1;
        }

        ret.data[..ret.nr as usize].sort_unstable_by_key(|&d| {
            let d = usize::from(d);
            (domain_keys[d], required.is_some_and(|r| !r.test(d)), self.next_alloc[d], d)
        });
    }
}

impl c::alloc_request {
    /// The devices placement requires of it - dev_alloc_required()'s - read
    /// now: None if none are.
    pub fn devs_required(&self) -> Option<c::DevsMask> {
        // SAFETY: null, or a slot of the filesystem's published Placement
        unsafe { self.devs_required.cast::<Volatile<c::DevsMask>>().as_ref() }.map(Volatile::get)
    }

    /// Whether taking a partial bucket on @dev leaves the copies still to
    /// place enough for the devices placement requires: the order is enough
    /// for fresh buckets, but partial ones are first come first served.
    pub(crate) fn required_leaves_room(&self, dev: u32) -> bool {
        let Some(required) = self.devs_required() else { return true };
        if required.test(dev as usize) {
            return true;
        }
        let nr_required = required.iter().filter(|&i| self.devs_may_alloc.d.test(i)).count() as u32;
        u32::from(self.nr_replicas).wrapping_sub(self.nr_effective) > nr_required
    }
}

impl Fs {
    /// @req's domain_keys, for every device it may allocate from - recomputed
    /// as each copy's chosen, which changes what the next should avoid. For
    /// erasure coding, failure domains are a hard requirement: a device
    /// sharing one with a chosen block is excluded.
    pub(crate) fn domain_keys_update(&self, req: &mut c::alloc_request) {
        let may = req.devs_may_alloc.d;
        {
            let rcu = read_lock();
            for i in may.iter() {
                req.domain_keys[i] = self.dev_domain_key(&req.devs_chosen, i as u32, &rcu);
            }
        }

        if req.bits.failure_domains_required() {
            for i in may.iter().filter(|&i| req.domain_keys[i] != 0) {
                req.devs_may_alloc.d.clear(i);
            }
        }
    }

    /// @req's devs_sorted: its devs_may_alloc, in the order to allocate from
    /// them, @stripe's hands breaking ties.
    pub fn dev_alloc_list(&self, stripe: &mut c::dev_stripe_state, req: &mut c::alloc_request) {
        self.domain_keys_update(req);
        let required = req.devs_required();
        stripe.alloc_list(&req.devs_may_alloc, &req.domain_keys, required.as_ref(), &mut req.devs_sorted);
    }

    /// The devices a replicated write of @req's has to put a copy on: when
    /// free space is lopsided enough at its count of copies that a device
    /// only fills if every write puts one on it - see DevFreeDist::required()
    /// - striping sometimes skipping it spends space reservations promised,
    /// and the last of it gets written degraded. Only when that can change a
    /// decision: @req may use one of them, and has more devices to choose
    /// from than copies left to place. Not for erasure coding, whose
    /// nr_replicas is a block count.
    pub(crate) fn dev_alloc_required(&self, req: &c::alloc_request) -> Option<&Volatile<c::DevsMask>> {
        let n = u32::from(req.nr_replicas);
        let p = &self.capacity.sectors_available_lock;

        if req.bits.ec() || !(2..=c::BCH_REPLICAS_MAX).contains(&n) ||
           p.placement_constrained.get() & 1 << (n - 1) == 0 {
            return None;
        }

        let required = &p.placement_required[n as usize - 1];
        let may = &req.devs_may_alloc.d;
        (required.get().intersects(may) && may.weight() > n.wrapping_sub(req.nr_effective)).then_some(required)
    }

    /// An allocation of @req's made on @dev while placement was restricted,
    /// @have its copies before it: counted, and traced - which builds its
    /// message in a printbuf, so not under a spinlock.
    pub(crate) fn placement_restricted_count(&self, req: &c::alloc_request, dev: u32, have: u32) {
        let Some(required) = req.devs_required() else { return };

        self.event_inc_trace(events::bucket_alloc_placement_restricted, |out| {
            write!(out, "{} replicas, have {have}, allocated on dev {dev} ({}), required:", req.nr_replicas,
                   if required.test(dev as usize) { "required" } else { "not required" });

            let rcu = read_lock();
            for i in required.iter() {
                let name = self.dev_rcu(i as u32, &rcu).map_or(c"(missing)", |ca| ca.name());
                write!(out, " {i} ({})", name.to_str().unwrap_or("?"));
            }
        });
    }
}

// ── For C ────────────────────────────────────────────────────────────────

/// The journal's: its domain keys in @domain_keys, not an alloc_request.
///
/// # Safety
/// @domain_keys is BCH_SB_MEMBERS_MAX u64s.
#[c_export]
pub unsafe fn bch2_dev_alloc_list_devs(c: &Opaque<c::bch_fs>, stripe: &mut c::dev_stripe_state,
                                       devs: &c::bch_devs_mask, devs_chosen: &c::bch_devs_mask,
                                       domain_keys: *mut u64, ret: &mut c::dev_alloc_list) {
    let fs = Fs::from_c(c);
    // SAFETY: the caller's
    let domain_keys = unsafe { &mut *domain_keys.cast::<[u64; NR_DEVS]>() };
    {
        let rcu = read_lock();
        for i in devs.d.iter() {
            domain_keys[i] = fs.dev_domain_key(devs_chosen, i as u32, &rcu);
        }
    }
    stripe.alloc_list(devs, domain_keys, None, ret);
}

#[c_export]
pub fn bch2_dev_stripe_increment(ca: &c::bch_dev, stripe: &mut c::dev_stripe_state) {
    stripe.increment(ca, &ca.usage_read_fast())
}
