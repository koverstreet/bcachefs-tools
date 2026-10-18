// SPDX-License-Identifier: GPL-2.0

//! A key's durability - how many device failures its data survives, plus one
//! - and how much of that is online: extents.c's.
//!
//! A pointer's durability is its device's - nothing for one that's gone or
//! being evacuated - but an erasure coded pointer's is its stripe's: the
//! stripe read from the stripes btree, so bkey_durability() takes a
//! transaction, and hands its restarts back up, to the caller's loop.
//!
//! A device fails once however many of the key's pointers, or of its stripes'
//! blocks, it backs, so it counts once: the total and the online sums are
//! deduplicated across the key, separately - they pick different devices.
//!
//! The counts are bkey_durability's u8s, summed as C's are: wrapping.

use crate::btree::bkey::{pos, BkeyI, BkeyS, BkeySC, BkeyV};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt};
use crate::c;
use crate::data::ec::trigger::{ptr_matches_stripe, stripe_block_ptrs};
use crate::data::ptrs::{crc_is_compressed, BkeyVPtrs, BkeyVPtrsMut, Ptrs, PtrsMut};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::ffi::Opaque;
use crate::util::inline_vec::InlineVec;
use crate::util::rcu::{read_lock, ReadGuard};
use arrayvec::ArrayVec;
use core::ffi::{c_int, c_uint};
use core::mem::MaybeUninit;
use cstruct_macros::c_export;

impl Fs {
    /// What a copy already on @dev is worth: nothing if the device is gone or
    /// being evacuated. bch2_dev_durability().
    pub fn dev_durability(&self, dev: u32, rcu: &ReadGuard) -> u32 {
        self.dev_rcu(dev, rcu)
            .filter(|ca| u32::from(ca.mi.state) != c::bch_member_state::BCH_MEMBER_STATE_evacuating.0)
            .map_or(0, |ca| ca.mi.durability.into())
    }

    /// What a copy on @dev will be worth once it's back: its durability,
    /// evacuating or not - bch2_dev_durability_desired().
    pub fn dev_durability_desired(&self, dev: u32, rcu: &ReadGuard) -> u32 {
        self.dev_rcu(dev, rcu).map_or(0, |ca| ca.mi.durability.into())
    }

    fn dev_online(&self, dev: u32) -> bool {
        self.devs_online.test(dev as usize)
    }
}

/// Devices already counted toward a key's durability, for the total and the
/// online sums.
#[derive(Default)]
struct Seen {
    total:  c::DevsMask,
    online: c::DevsMask,
}

/// A stripe block's device, as stripe_durability() sorts them.
#[derive(Clone, Copy)]
struct StripeDev {
    dev:                u8,
    durability:         u8,
    durability_desired: u8,
    online:             bool,
}

/// What erasure coded pointer @p's stripe adds to @ret, @seen the devices
/// already counted, and to @desired_total: bch2_stripe_durability().
///
/// The data survives losing nr_redundant of the stripe's nr_blocks devices,
/// so its total durability is the sum of the nr_redundant + 1 least durable
/// devices'. It's readable now with nr_data of them online, so its online
/// durability is the sum of the (online - nr_data + 1) least durable online
/// devices' - none with fewer than nr_data online. A device @seen has counted
/// fills its place, and isn't summed again. The desired sum is deduplicated
/// within the stripe, not by @seen: it's per pointer, for accounting.
fn stripe_durability(t: &TransAttempt<'_, '_>, p: &c::extent_ptr_decoded, ret: &mut c::bkey_durability,
                     seen: &mut Seen, desired_total: Option<&mut u32>) -> Result<(), BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, p.ec.idx()), BtreeIterFlags::CACHED);
    let Some(k) = iter.peek_slot(t)? else { return Ok(()) };
    let Some(s) = k.arm::<c::Stripe>() else { return Ok(()) };

    // A stale stripe pointer gets nothing, as the stripe_ptr trigger doesn't
    // account it: else we'd report redundancy the data doesn't have
    if !ptr_matches_stripe(s, p) {
        return Ok(());
    }

    let v = s.v();

    // nothing allocates in the read-side section
    let mut devs = InlineVec::<StripeDev, { c::BCH_BKEY_PTRS_MAX as usize }>::new();
    devs.reserve(v.nr_blocks.into(), GFP_KERNEL)?;
    {
        let rcu = read_lock();
        for sp in stripe_block_ptrs(s) {
            let dev = u32::from(sp.dev());
            devs.push(StripeDev {
                dev:                sp.dev(),
                durability:         fs.dev_durability(dev, &rcu) as u8,
                durability_desired: fs.dev_durability_desired(dev, &rcu) as u8,
                online:             fs.dev_online(dev),
            }, GFP_KERNEL).expect("pushing within reserved room doesn't allocate");
        }
    }

    stripe_devs_durability(&mut devs, v.nr_blocks, v.nr_redundant, ret, seen, desired_total);
    Ok(())
}

/// Sorted by @key, stably - as C's insertion sort: of equal keys, the first
/// in block order first.
fn sort_stable_by_key(devs: &mut [StripeDev], key: impl Fn(&StripeDev) -> u8) {
    for i in 1..devs.len() {
        let d = devs[i];
        let mut at = i;
        while at > 0 && key(&d) < key(&devs[at - 1]) {
            devs[at] = devs[at - 1];
            at -= 1;
        }
        devs[at] = d;
    }
}

/// stripe_durability()'s sums, over @devs, its blocks' devices in block order
/// - left sorted.
///
/// Of devices of equal durability, the first in block order are taken first:
/// that decides which @seen counts, so it's C's order exactly.
fn stripe_devs_durability(devs: &mut [StripeDev], nr_blocks: u8, nr_redundant: u8,
                          ret: &mut c::bkey_durability, seen: &mut Seen, desired_total: Option<&mut u32>) {
    // unsigned, as C's: nr_redundant past nr_blocks is damage
    let nr_data = u32::from(nr_blocks).wrapping_sub(nr_redundant.into());
    let online_count = devs.iter().filter(|d| d.online).count() as u32;

    let total_keep = u32::from(nr_redundant) + 1;
    let online_keep = online_count as i32 - nr_data as i32 + 1;
    let mut online_taken = 0u32;

    sort_stable_by_key(devs, |d| d.durability);
    for (i, d) in devs.iter().enumerate() {
        if (i as u32) < total_keep && !seen.total.test_and_set(d.dev.into()) {
            ret.total = ret.total.wrapping_add(d.durability);
        }

        if online_keep > 0 && d.online && online_taken < online_keep as u32 {
            online_taken += 1;
            if !seen.online.test_and_set(d.dev.into()) {
                ret.online = ret.online.wrapping_add(d.durability);
            }
        }
    }

    if let Some(desired_total) = desired_total {
        sort_stable_by_key(devs, |d| d.durability_desired);

        let mut desired_seen = c::DevsMask::default();
        for d in devs.iter().take(total_keep as usize) {
            if !desired_seen.test_and_set(d.dev.into()) {
                *desired_total += u32::from(d.durability_desired);
            }
        }
    }
}

/// A pointer's durability: none for a cached one, its stripe's for an
/// erasure coded one. bch2_extent_ptr_durability().
pub fn extent_ptr_durability(t: &TransAttempt<'_, '_>, p: &c::extent_ptr_decoded) -> Result<u32, BchError> {
    if p.ptr.cached() != 0 {
        return Ok(0);
    }

    let fs = t.fs();
    if !p.has_ec {
        return Ok(fs.dev_durability(p.ptr.dev().into(), &read_lock()));
    }

    let mut d = c::bkey_durability::default();
    stripe_durability(t, p, &mut d, &mut Seen::default(), None)?;
    Ok(d.total.into())
}

/// A reservation's: no pointers, but a replica count - all of it online, a
/// claim on free space.
fn reservation_durability(k: BkeySC<'_>) -> Option<c::bkey_durability> {
    let nr = k.arm::<c::Reservation>()?.v().nr_replicas;
    Some(c::bkey_durability {
        online:          nr,
        total:           nr,
        nr_overwritable: nr,
        nr_replicas:     nr,
        replicas:        nr,
        min_durability:  u8::MAX,
        ..Default::default()
    })
}

/// The counts of @p's that need no device: its pointer, replicas, and how
/// much is compressed.
fn count_ptr(ret: &mut c::bkey_durability, p: &c::extent_ptr_decoded) {
    let real = u8::from(u32::from(p.ptr.dev()) != c::BCH_SB_MEMBER_INVALID);
    let compressed = crc_is_compressed(&p.crc);

    ret.nr_ptrs = ret.nr_ptrs.wrapping_add(real);
    ret.nr_replicas = ret.nr_replicas.wrapping_add(real);
    ret.replicas = ret.replicas.wrapping_add(1).wrapping_add(if p.has_ec { p.ec.redundancy() } else { 0 });
    ret.nr_overwritable = ret.nr_overwritable.wrapping_add(u8::from(!compressed));
    if compressed {
        ret.sectors_compressed = ret.sectors_compressed.wrapping_add(p.crc.compressed_size);
    }
}

impl BkeySC<'_> {
    /// Its durability, total and online - each device counted once - with its
    /// replica and pointer counts, the per-pointer sum of desired durability
    /// (acct) and the least of it (min_durability, an evacuating device's
    /// none): bch2_bkey_durability(). Its stripes are looked up in @t.
    pub fn durability(self, t: &TransAttempt<'_, '_>) -> Result<c::bkey_durability, BchError> {
        if let Some(r) = reservation_durability(self) {
            return Ok(r);
        }

        let mut ret = c::bkey_durability { min_durability: u8::MAX, ..Default::default() };
        let Some(k) = self.arm::<Ptrs>() else { return Ok(ret) };
        let fs = t.fs();
        let mut seen = Seen::default();

        for d in k.ptrs_decoded(fs) {
            let p = &d.p;
            if p.ptr.cached() != 0 {
                continue;
            }
            count_ptr(&mut ret, p);

            let dev = u32::from(p.ptr.dev());
            let mut desired = 0;

            if p.has_ec {
                // its stripe's, even as a BCH_SB_MEMBER_INVALID placeholder
                stripe_durability(t, p, &mut ret, &mut seen, Some(&mut desired))?;
            } else if dev != c::BCH_SB_MEMBER_INVALID {
                let rcu = read_lock();
                let cur = fs.dev_durability(dev, &rcu) as u8;
                desired = fs.dev_durability_desired(dev, &rcu);

                if !seen.total.test_and_set(dev as usize) {
                    ret.total = ret.total.wrapping_add(cur);
                }
                if fs.dev_online(dev) && !seen.online.test_and_set(dev as usize) {
                    ret.online = ret.online.wrapping_add(cur);
                }
            }

            ret.acct = ret.acct.wrapping_add(desired as u8);
            let d_min = if !p.has_ec && fs.dev_bad_or_evacuating(dev, &read_lock()) { 0 } else { desired };
            ret.min_durability = u32::from(ret.min_durability).min(d_min) as u8;
        }
        Ok(ret)
    }

    /// durability() without the stripe lookups, so without a transaction: an
    /// erasure coded pointer counts as its redundancy plus one, and online
    /// isn't counted. bch2_bkey_durability_safe(), for reserving space to
    /// rewrite a key: tolerates bad pointers.
    pub fn durability_safe(self, fs: &Fs) -> c::bkey_durability {
        if let Some(r) = reservation_durability(self) {
            return r;
        }

        let mut ret = c::bkey_durability { min_durability: u8::MAX, ..Default::default() };
        let Some(k) = self.arm::<Ptrs>() else { return ret };
        let rcu = read_lock();

        for d in k.ptrs_decoded(fs) {
            let p = &d.p;
            if p.ptr.cached() != 0 {
                continue;
            }
            count_ptr(&mut ret, p);

            let dev = u32::from(p.ptr.dev());
            if p.has_ec {
                ret.total = ret.total.wrapping_add(p.ec.redundancy()).wrapping_add(1);
            } else if dev != c::BCH_SB_MEMBER_INVALID {
                let d = fs.dev_rcu(dev, &rcu).map_or(1, |ca| ca.mi.durability);
                ret.total = ret.total.wrapping_add(d);
            }
        }
        ret
    }

    /// A btree node pointer's durability, total and online - each pointer's
    /// device counted, unlike a data key's, as often as it's pointed to:
    /// bch2_btree_ptr_durability().
    pub fn btree_ptr_durability(self, fs: &Fs) -> c::bkey_durability {
        use c::bch_bkey_type as t;
        assert!(matches!(self.k.key_type(), t::KEY_TYPE_btree_ptr | t::KEY_TYPE_btree_ptr_v2),
                "btree_ptr_durability() of a {:?}", self.k.key_type());

        let mut ret = c::bkey_durability::default();
        let k = self.arm::<Ptrs>().expect("a btree node pointer has pointers");
        let rcu = read_lock();

        for ptr in k.ptrs(fs) {
            let dev = u32::from(ptr.dev());
            if ptr.cached() == 0 && dev != c::BCH_SB_MEMBER_INVALID {
                let d = fs.dev_durability(dev, &rcu) as u8;
                if fs.dev_online(dev) {
                    ret.online = ret.online.wrapping_add(d);
                }
                ret.total = ret.total.wrapping_add(d);
            }
        }
        ret
    }

    /// Whether any of it can be read: a pointer not cached, to a device or
    /// with a stripe - bch2_bkey_can_read().
    pub fn can_read(self, fs: &Fs) -> bool {
        self.arm::<Ptrs>().is_some_and(|k| {
            k.ptrs_decoded(fs).any(|d| {
                d.p.ptr.cached() == 0 &&
                    (u32::from(d.p.ptr.dev()) != c::BCH_SB_MEMBER_INVALID || d.p.has_ec)
            })
        })
    }
}

// ── dropping what isn't needed ───────────────────────────────────────────

/// Of @k's pointers in @mask, by index, the ones it can do without, dropped
/// (@kill) or made cached: bch2_bkey_drop_extra_durability(). Each is tried
/// on a copy, the whole key's online durability recomputed - a per-pointer
/// delta can't count devices pointers share.
pub fn drop_extra_durability(t: &TransAttempt<'_, '_>, opts: &c::bch_inode_opts, mut k: BkeyS<'_, PtrsMut>,
                             mask: u32, kill: bool) -> Result<(), BchError> {
    if mask == 0 {
        return Ok(());
    }
    let fs = t.fs();
    let mut n = t.bkey_make_mut_noupdate(k.as_sc())?;
    let nr_ptrs = n.ptrs(fs).count().min(32);
    let mut ptrs_kill = 0u32;

    for i in 0..nr_ptrs {
        let bit = 1u32 << i;
        let ptr = n.ptrs_mut(fs).nth(i).expect("a pointer counted");
        if mask & bit == 0 || ptr.cached() != 0 {
            continue;
        }
        ptr.set_cached(1);

        if n.as_sc().untyped().durability(t)?.online >= opts.data_replicas {
            ptrs_kill |= bit;
        } else {
            n.ptrs_mut(fs).nth(i).expect("a pointer counted").set_cached(0);
        }
    }

    if kill {
        k.drop_ptrs_mask(fs, ptrs_kill);
    } else {
        k.set_ptrs_cached_mask(fs, opts, ptrs_kill);
    }
    Ok(())
}

/// Of @k's pointers in @mask, the stripe_ptrs it can do without, dropped -
/// tried on a copy, as drop_extra_durability(): bch2_bkey_drop_extra_ec_durability().
/// The devices are gathered first: dropping a stripe_ptr moves the entries.
pub fn drop_extra_ec_durability(t: &TransAttempt<'_, '_>, opts: &c::bch_inode_opts, mut k: BkeyS<'_, PtrsMut>,
                                mask: u32) -> Result<(), BchError> {
    if mask == 0 {
        return Ok(());
    }
    let fs = t.fs();
    let devs: ArrayVec<u32, { c::BCH_BKEY_PTRS_MAX as usize }> = k.ptrs(fs).enumerate()
        .filter(|&(i, _)| i < 32 && mask & (1 << i) != 0)
        .map(|(_, p)| u32::from(p.dev()))
        .collect();

    for dev in devs {
        let mut n = t.bkey_make_mut_noupdate(k.as_sc())?;
        n.drop_ec(fs, dev);

        if n.as_sc().untyped().durability(t)?.online >= opts.data_replicas {
            k.drop_ec(fs, dev);
        }
    }
    Ok(())
}

// ── C's ──────────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_dev_durability(c: &Opaque<c::bch_fs>, dev: c_uint) -> c_uint {
    Fs::from_c(c).dev_durability(dev, &read_lock())
}

/// # Safety
/// C has an attempt in progress on @trans.
#[c_export]
pub unsafe fn bch2_extent_ptr_durability(trans: &BtreeTrans<'_>, p: &c::extent_ptr_decoded) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    extent_ptr_durability(&t, p).map_or_else(|e| -e.raw(), |d| d as c_int)
}

/// @ret is written, not read.
///
/// # Safety
/// C has an attempt in progress on @trans.
#[c_export]
pub unsafe fn bch2_bkey_durability(trans: &BtreeTrans<'_>, k: BkeySC<'_>,
                                   ret: &mut MaybeUninit<c::bkey_durability>) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    match k.durability(&t) {
        Ok(d) => {
            ret.write(d);
            0
        }
        Err(e) => -e.raw(),
    }
}

#[c_export]
pub fn bch2_btree_ptr_durability(c: &Opaque<c::bch_fs>, k: BkeySC<'_>) -> c::bkey_durability {
    k.btree_ptr_durability(Fs::from_c(c))
}

#[c_export]
pub fn bch2_bkey_can_read(c: &Fs, k: BkeySC<'_>) -> bool {
    k.can_read(c)
}

#[c_export]
pub fn bch2_bkey_durability_safe(c: &Fs, k: BkeySC<'_>) -> c::bkey_durability {
    k.durability_safe(c)
}

/// @k, a key C hands as a struct bkey_i *, if it has pointers - which then
/// must be ones that can be edited.
///
/// # Safety
/// @k is a key, all of it, the caller's to edit for 'a.
unsafe fn bkey_i_ptrs_mut<'a>(k: *mut c::bkey_i) -> Option<BkeyS<'a, PtrsMut>> {
    // SAFETY: the caller's
    let k = BkeyS::from(unsafe { &mut *BkeyI::from_raw_mut(k) });
    k.as_sc().arm::<Ptrs>()?;
    Some(k.arm::<PtrsMut>().expect("dropping the extra durability of a key whose pointers can't be edited"))
}

/// A key without pointers has none to drop.
///
/// # Safety
/// C has an attempt in progress on @trans, and @k is a key it can edit.
#[c_export]
pub unsafe fn bch2_bkey_drop_extra_durability(trans: &BtreeTrans<'_>, opts: &c::bch_inode_opts,
                                              k: *mut c::bkey_i, mask: c_uint, kill: bool) -> c_int {
    // SAFETY: the caller's
    let (t, k) = unsafe { (trans.attempt_in_progress(), bkey_i_ptrs_mut(k)) };
    k.map_or(0, |k| ret_to_c(drop_extra_durability(&t, opts, k, mask, kill)))
}

/// A key without pointers has none to drop.
///
/// # Safety
/// C has an attempt in progress on @trans, and @k is a key it can edit.
#[c_export]
pub unsafe fn bch2_bkey_drop_extra_ec_durability(trans: &BtreeTrans<'_>, opts: &c::bch_inode_opts,
                                                 k: *mut c::bkey_i, mask: c_uint) -> c_int {
    // SAFETY: the caller's
    let (t, k) = unsafe { (trans.attempt_in_progress(), bkey_i_ptrs_mut(k)) };
    k.map_or(0, |k| ret_to_c(drop_extra_ec_durability(&t, opts, k, mask)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block on @dev, of @durability - @desired once it's back - online or
    /// not.
    fn dev(dev: u8, durability: u8, desired: u8, online: bool) -> StripeDev {
        StripeDev { dev, durability, durability_desired: desired, online }
    }

    /// stripe_devs_durability() of @devs, a fresh key's first stripe: its
    /// total, online and desired durability.
    fn sums(mut devs: std::vec::Vec<StripeDev>, nr_redundant: u8, seen: &mut Seen) -> (u8, u8, u32) {
        let mut ret = c::bkey_durability::default();
        let mut desired = 0;
        let nr_blocks = devs.len() as u8;
        stripe_devs_durability(&mut devs, nr_blocks, nr_redundant, &mut ret, seen, Some(&mut desired));
        (ret.total, ret.online, desired)
    }

    #[test]
    fn redundancy_plus_one() {
        // 4+2, every device durability 1 and online: survives two failures
        let devs = (0..6).map(|i| dev(i, 1, 1, true)).collect();
        assert_eq!(sums(devs, 2, &mut Seen::default()), (3, 3, 3));
    }

    #[test]
    fn online_counts_what_can_be_read_now() {
        // one of six offline: one failure from unreadable now, two in all
        let devs = (0..6).map(|i| dev(i, 1, 1, i != 3)).collect();
        assert_eq!(sums(devs, 2, &mut Seen::default()), (3, 2, 3));

        // fewer than nr_data online: none of it readable now
        let devs = (0..6).map(|i| dev(i, 1, 1, i < 3)).collect();
        assert_eq!(sums(devs, 2, &mut Seen::default()).1, 0);
    }

    #[test]
    fn least_durable_devices_sum() {
        // the three least durable of 2,1,2,1,1,2
        let devs = [2, 1, 2, 1, 1, 2].iter().enumerate()
            .map(|(i, &d)| dev(i as u8, d, d, true)).collect();
        assert_eq!(sums(devs, 2, &mut Seen::default()).0, 3);
    }

    #[test]
    fn a_device_counts_once() {
        // two blocks on dev 0, among the three least durable: it fails once
        let devs = vec![dev(0, 1, 1, true), dev(0, 1, 1, true), dev(1, 1, 1, true), dev(2, 1, 1, true)];
        assert_eq!(sums(devs, 2, &mut Seen::default()), (2, 2, 2));
    }

    #[test]
    fn devices_another_stripe_counted_fill_their_place() {
        // dev 1 counted already, by another of the key's pointers: it still
        // takes its place among the least durable, and isn't summed again
        let mut seen = Seen::default();
        seen.total.set(1);
        seen.online.set(1);
        let devs = (0..4).map(|i| dev(i, 1, 1, true)).collect();
        let (total, online, desired) = sums(devs, 1, &mut seen);
        assert_eq!((total, online), (1, 1));
        assert_eq!(desired, 2, "desired is per pointer - not deduplicated by @seen");
    }

    #[test]
    fn equal_durability_in_block_order() {
        // A and C online, B not; nr_data 2, so one online place: A's, the
        // first in block order - and with A counted already, none summed
        let mut seen = Seen::default();
        seen.online.set(0);
        let devs = vec![dev(0, 1, 1, true), dev(1, 1, 1, false), dev(2, 1, 1, true)];
        let (total, online, _) = sums(devs, 1, &mut seen);
        assert_eq!(online, 0, "the online place is A's, not C's");
        assert_eq!(total, 2, "A and B, the first two");
    }

    #[test]
    fn desired_by_desired_durability() {
        // an evacuating device: worth nothing now, its durability once back
        let devs = vec![dev(0, 0, 2, true), dev(1, 1, 1, true), dev(2, 1, 1, true)];
        let (total, _, desired) = sums(devs, 1, &mut Seen::default());
        assert_eq!(total, 1, "now: the evacuating device's 0, and a 1");
        assert_eq!(desired, 2, "back: the two 1s, the least desired");
    }
}
