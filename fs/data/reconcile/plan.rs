// SPDX-License-Identifier: GPL-2.0

//! What reconcile does to one key. A key with work has a reconcile entry: the
//! options it should be in line with, and need_rb, which of them it isn't. Its
//! plan is one of:
//!
//! - Move: a data update, its data_update_opts saying which pointers to kill
//!   - ptrs_kill: the replica goes; ptrs_kill_ec: the pointer stays, its
//!   stripe pointer goes - and whether to write a replica for a stripe to
//!   take in (extra_replicas, must_ec).
//! - Park: nothing can be done until the configuration changes - no stripe
//!   can form, data_replicas can't be matched exactly. The entry's marked
//!   pending, which moves the key to the pending work btree: walked only
//!   after a pending scan, which a device added, removed, resized, relabelled
//!   or changing state requests.
//! - Nothing: a race with the write buffer, or a stripe being made from one
//!   of its buckets.
//!
//! plan() reads the key and the devices, and changes nothing: parking is its
//! caller's, park(), as is the move.
//!
//! # Dropping replicas
//!
//! An over-replicated key - durability past data_replicas - has pointers to
//! drop, and ptrs_to_drop() picks them: the drop simulation. Every drop it
//! proposes must be one the update's apply path takes
//! (drop_extra_durability()), or the work never completes and reconcile
//! respins on it. So it drops on a copy of the key, measuring the copy's
//! whole durability after each - not the pointer's own, which would count
//! twice a device two pointers share, and miss what a stripe adds.
//!
//! Two phases: pointers on offline devices, holding total durability at
//! data_replicas - only if some durability's offline - then any pointer,
//! holding online durability. In each, whole pointers first, dropped by
//! marking them cached: a reversible exclusion, as a cached pointer counts for
//! nothing. Then stripe pointers - gathered first, as dropping one rewrites the
//! key's entries - each measured on a copy of its own. A pointer goes if the
//! floor holds without it, and it's on a bad or evacuating device or it
//! actually counted: one that didn't is a cache replica, on a durability 0
//! device, not ours to drop. An evacuating device's pointer counts nothing
//! too, so it goes once others hold the durability.
//!
//! The property to verify: every pointer ptrs_to_drop() marks, the apply path
//! drops - given the same devices. Devices that change between plan and
//! update are can_do_data_update()'s, which decides at the update.
//!
//! # Masks
//!
//! ptrs_kill and ptrs_kill_ec are PtrMasks of the key planned for, made and
//! combined under its brand, the drop simulation's copies adopted as it
//! (ptrs_mask.rs). They're erased into data_update_opts' u8s: a pointer past
//! the eighth has no bit there, as in C.

use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::{BkeyI, BkeySC, BkeyV};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{is_restart, BtreeIter, BtreeTrans, CommitFlags, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::data::durability::BkeyVDurability;
use crate::data::ptrs::{brand, BkeyVPtrs, Branded, ExtentEntryMut, PtrMask, Ptrs, PtrsMut};
use crate::data::reconcile::needs_reconcile::rb_bit;
use crate::data::reconcile::trigger::{rb_work_id, BkeyVReconcile};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::{log_msg_ratelimited, warn_on_once};
use crate::sb::counters::events;
use crate::util::rcu::read_lock;
use crate::util::vstructs::plain_bytes;
use core::ffi::{c_int, c_uint};
use core::mem::size_of;
use cstruct_macros::c_export;

/// Why a key's parked, for the trace.
#[derive(Clone, Copy)]
pub enum ParkReason {
    /// No stripe of @redundancy can form in @target now.
    NoStripe { target: u32, redundancy: u32 },
    /// data_replicas can't be matched: the devices' durabilities don't sum
    /// to it, or a drop would cost online durability.
    Replicas(u32),
}

pub enum Plan {
    Move(c::data_update_opts),
    Park(ParkReason),
    Nothing,
}

/// The plan for @k, its io options @opts: reconcile_set_data_opts().
pub fn plan(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, opts: &c::bch_inode_opts) -> Result<Plan, BchError> {
    use c::bch_reconcile_opts as o;
    use c::bch_write_flags as w;
    let fs = t.fs();

    let Some(kp) = k.arm::<Ptrs>() else { return Ok(Plan::Nothing) };
    // none: a write buffer race
    let Some(&r) = kp.reconcile_opts(fs).filter(|r| r.need_rb() != 0) else { return Ok(Plan::Nothing) };
    let need = |opt| r.need_rb() & rb_bit(opt) != 0;
    let only = |opt| r.need_rb() == rb_bit(opt);

    let mut d = c::data_update_opts {
        type_: c::bch_data_update_types::BCH_DATA_UPDATE_reconcile,
        target: r.background_target(),
        // Never wait on the allocator mid-write: a blocked data update holds
        // a read-time snapshot of the extent while other movers rewrite it,
        // and the stale write is then discarded at index update time
        // (data_update_useless_write_fail). Better to fail with
        // freelist_empty and retry from a fresh read.
        write_flags: w::BCH_WRITE_alloc_nowait,
        ..Default::default()
    };
    // Replicas can't be added to or dropped from btree nodes incrementally:
    // they always need to be able to spill over to the whole filesystem
    if r.hipri() == 0 && !k.k.is_btree_ptr() {
        d.write_flags |= w::BCH_WRITE_only_specified_devs;
    }

    // An unknown checksum or compression type in the entry is an incompat
    // feature and shouldn't have mounted, so this is only reachable if our
    // versioning was wrong - and getting it wrong would otherwise panic
    // converting it. The io options have had the same fallback: safe to take.
    let mut rb = r;
    if u32::from(rb.data_checksum()) >= c::bch_csum_opt::BCH_CSUM_OPT_NR.0 {
        rb.set_data_checksum(opts.data_checksum);
    }
    if !c::bch_compression_opt::valid(rb.background_compression()) {
        rb.set_background_compression(opts.background_compression);
    }
    let csum_type = fs.data_checksum_type_rb(&rb).0 as u8;
    let compression_type = c::bch_compression_opt::to_type(rb.background_compression(), fs).0 as u8;

    // Before the parking below, which doesn't need them: they're only read
    let (kill, kill_ec) = brand(kp, |kb| -> Result<_, BchError> {
        let (mut kill, mut kill_ec) = if need(o::BCH_RECONCILE_data_replicas) {
            ptrs_to_drop(t, &kb, &r)?
        } else {
            (kb.none(), kb.none())
        };

        if need(o::BCH_RECONCILE_erasure_code) && r.erasure_code() == 0 {
            kill_ec |= kb.where_(fs, |p| p.p.has_ec);
        }

        let rcu = read_lock();
        let target = crate::alloc::disk_groups::Target::decode(r.background_target().into());
        kill |= kb.where_(fs, |p| {
            (need(o::BCH_RECONCILE_data_checksum) && p.p.crc.csum_type != csum_type) ||
            (need(o::BCH_RECONCILE_background_compression) && p.p.crc.compression_type != compression_type) ||
            (need(o::BCH_RECONCILE_background_target) && p.p.ptr.cached() == 0 &&
             !fs.dev_in_target(p.p.ptr.dev().into(), target, &rcu))
        });
        Ok((kill.bits(), kill_ec.bits()))
    })?;

    if need(o::BCH_RECONCILE_erasure_code) && r.erasure_code() != 0 {
        let target = r.background_target().into();
        let redundancy = u32::from(r.data_replicas()).wrapping_sub(1);

        // If no stripe can form - too few RW devices in the target with
        // a matching bucket size - an update would fail and requeue
        // forever. With EC all there is to do, park: a device change
        // will look again. Otherwise do the rest.
        if !fs.can_form_ec_stripe(target, redundancy, None) {
            if only(o::BCH_RECONCILE_erasure_code) {
                return Ok(Plan::Park(ParkReason::NoStripe { target, redundancy }));
            }
        } else {
            // XXX: we'll need ratelimiting
            if ec_pending(fs, &kp) {
                return Ok(Plan::Nothing);
            }

            d.extra_replicas = 1;
            d.bits.set_no_devs_have(true);
            if only(o::BCH_RECONCILE_erasure_code) {
                d.write_flags |= w::BCH_WRITE_must_ec;
            }
        }
    }

    d.ptrs_kill    = kill as u8;
    d.ptrs_kill_ec = kill_ec as u8;
    if d.ptrs_kill != 0 || d.ptrs_kill_ec != 0 || d.extra_replicas != 0 {
        return Ok(Plan::Move(d));
    }

    // Devices all durability 2 and replicas 1 or 3 - we can't exactly
    // match the replicas setting - or replicas to drop that we can't
    // without reducing online durability
    if only(o::BCH_RECONCILE_data_replicas) {
        return Ok(Plan::Park(ParkReason::Replicas(r.data_replicas().into())));
    }

    // A bug: the key's reconcile entry asks for work this finds none of. The
    // warning fails the CI test that gets here, making it a reproducer - once,
    // as the same keys come back every pass.
    {
        let mut msg = log_msg_ratelimited!(fs);
        write!(msg, "got extent to reconcile but nothing to do, confused\n  {}", k.to_text(fs));
    }
    warn_on_once!(true);
    Ok(Plan::Nothing)
}

/// Whether a stripe being made has a block in a bucket of @k's:
/// extent_ec_pending().
fn ec_pending(fs: &Fs, k: &impl BkeyVPtrs) -> bool {
    let rcu = read_lock();
    k.ptrs(fs).any(|p| fs.dev_rcu(p.dev().into(), &rcu).is_some_and(|ca| fs.bucket_has_new_stripe(ca.ptr_bucket_pos(p))))
}

/// @k's pointers to drop for data_replicas @r: kill and kill_ec, as masks.
/// At or under: those on bad or evacuating devices. Over: the drop
/// simulation, at the top of the file.
fn ptrs_to_drop<'id>(t: &TransAttempt<'_, '_>, k: &Branded<'id, BkeySC<'_, Ptrs>>, r: &c::bch_extent_reconcile)
    -> Result<(PtrMask<'id>, PtrMask<'id>), BchError>
{
    let fs = t.fs();
    let want = u32::from(r.data_replicas());
    let durability = k.key().untyped().durability(t)?;

    if u32::from(durability.total) <= want {
        let rcu = read_lock();
        return Ok((k.where_(fs, |p| fs.dev_bad_or_evacuating(p.ptr.dev().into(), &rcu)), k.none()));
    }

    let offline = |dev: u32| dev == c::BCH_SB_MEMBER_INVALID || !fs.devs_online.test(dev as usize);

    let mut sim = Sim::new(k.key().untyped());
    let mut n = k.adopt(fs, sim.key_mut());
    let mut cur = durability;
    let (mut kill, mut kill_ec) = (k.none(), k.none());

    for online_floor in [false, true] {
        // holding the total only matters if some durability's offline
        if !online_floor && cur.total == cur.online {
            continue;
        }
        let floor = |d: &c::bkey_durability| u32::from(if online_floor { d.online } else { d.total });

        for (i, bit) in n.all(fs).iter() {
            let ptr = *n.key().ptrs(fs).nth(i).expect("a pointer of the copy's");
            let dev = u32::from(ptr.dev());
            if ptr.cached() != 0 || !(online_floor || offline(dev)) {
                continue;
            }

            let force = fs.dev_bad_or_evacuating(dev, &read_lock());
            n.ptrs_mut(fs).nth(i).expect("a pointer of the copy's").set_cached(1);
            let d = n.key().untyped().durability(t)?;

            if floor(&d) >= want && (force || floor(&d) < floor(&cur)) {
                kill |= bit;
                cur = d;
            } else {
                n.ptrs_mut(fs).nth(i).expect("a pointer of the copy's").set_cached(0);
            }
        }

        let ec = n.where_(fs, |p| p.p.has_ec && p.p.ptr.cached() == 0 && (online_floor || offline(p.p.ptr.dev().into())));

        for (_, bit) in ec.iter() {
            let mut sim = Sim::new(n.key().untyped());
            let mut m = n.adopt(fs, sim.key_mut());
            m.drop_ec_mask(fs, bit);
            let d = m.key().untyped().durability(t)?;

            if floor(&d) >= want {
                kill_ec |= bit;
                n.drop_ec_mask(fs, bit);
                cur = d;
            }
        }
    }
    Ok((kill, kill_ec))
}

/// A copy of a key with pointers, to drop them from: the drop simulation's.
/// Its masks are the key's it's a copy of: it's adopted, as that key's brand.
struct Sim(BkeyBuf);

impl Sim {
    fn new(k: BkeySC<'_>) -> Self {
        Sim(BkeyBuf::from_key(k))
    }

    fn key_mut(&mut self) -> &mut BkeyI<PtrsMut> {
        self.0.as_mut().arm::<PtrsMut>().expect("a copy of a key with pointers")
    }
}

/// What a pending key counts as, in the pending sectors: a btree node
/// pointer its whole node.
pub fn pending_sectors(fs: &Fs, k: BkeySC<'_>) -> u64 {
    if k.k.is_btree_ptr() { fs.btree_sectors().into() } else { k.k.size.into() }
}

/// Park @k, at @iter at @level, for @why: marked pending, and traced.
pub fn park(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, level: u32, k: BkeySC<'_>, why: ParkReason)
    -> Result<(), BchError>
{
    let fs = t.fs();
    fs.event_add_trace(events::reconcile_set_pending, pending_sectors(fs, k), |out| match why {
        ParkReason::NoStripe { target, redundancy } => {
            write!(out, "can't form ec stripe\n{}\n", k.to_text(fs));
            fs.can_form_ec_stripe(target, redundancy, Some(out));
        }
        ParkReason::Replicas(want) => write!(out, "can't match data_replicas={want}\n{}", k.to_text(fs)),
    });
    pending_mod(t, iter, level, k, true)
}

/// Whether move error @e parks @k rather than failing it: no devices to
/// write to, too few, no space - the configuration has to change first.
/// Traced, with why the update can't be done; Err(@e) if it doesn't park.
/// Not a transient EC allocation failure: ENOSPC-class, as allocation
/// failures are, but it says "not right now", and nothing retries the
/// pending list. check_reconcile_pending_err().
pub fn parks_on(t: &TransAttempt<'_, '_>, opts: &c::bch_inode_opts, data_opts: &c::data_update_opts,
                k: BkeySC<'_>, e: BchError) -> Result<(), BchError> {
    use c::bch_errcode as err;
    let fs = t.fs();

    if e.matches(err::BCH_ERR_ec_alloc_failed_transient) ||
        !(e.matches(err::BCH_ERR_data_update_fail_no_rw_devs) ||
          e.matches(err::BCH_ERR_insufficient_devices) ||
          e.matches(c::ENOSPC)) {
        return Err(e);
    }

    // Why, as can_do_data_update() tells the trace - which can restart
    // the attempt, getting a stripe head: that's then ours to return
    let mut restart = None;
    fs.event_add_trace(events::reconcile_set_pending, pending_sectors(fs, k), |out| {
        write!(out, "{e}\n{}\n", k.to_text(fs));
        fs.data_update_opts_to_text(out, opts, data_opts);
        write!(out, "\n");
        restart = t.can_do_data_update(opts, data_opts, k, Some(out)).err().filter(is_restart);
    });
    restart.map_or(Ok(()), Err)
}

/// @k's reconcile entry marked pending, if @set, or not - committed, @k at
/// @iter; at @level, a btree node pointer, its node's key updated in place.
/// Nothing without work, or if it's already so.
/// bch2_extent_reconcile_pending_mod().
pub fn pending_mod(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, level: u32, k: BkeySC<'_>, set: bool)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let Some(r) = k.arm::<Ptrs>().and_then(|kp| kp.reconcile_opts(fs).copied()) else { return Ok(()) };
    if r.need_rb() == 0 || (rb_work_id(Some(&r)) == c::reconcile_work_id::RECONCILE_WORK_pending) == set {
        return Ok(());
    }

    t.relock()?;

    let set_pending = |n: &mut TransBkey<'_, '_>| {
        let mut n = n.reborrow().arm::<PtrsMut>().expect("a key with a reconcile entry has pointers");
        let r = n.extent_entries_mut(fs).find_map(|e| match e {
            ExtentEntryMut::reconcile(r) => Some(r),
            _ => None,
        });
        r.expect("the reconcile entry it was read with").set_pending(set.into());
    };

    if level == 0 {
        let mut n = t.bkey_reassemble_room(k, c::BKEY_EXTENT_U64s_MAX as usize * size_of::<u64>())?;
        set_pending(&mut n);
        t.update(iter, &n, UpdateTriggerFlags::empty())?;
        return t.commit(Some(&DiskReservation::new(fs)), CommitFlags::NO_ENOSPC);
    }

    t.btree_node_edit_key(iter.btree(), level, k.k.p, |n| {
        // the node's key is the pointer we were handed, or the interior
        // update machinery or the iterators have gone badly wrong
        if !node_key_eq(n.as_sc(), k) {
            panic!("\n{}\n{}", n.as_sc().to_text(fs), k.to_text(fs));
        }
        set_pending(n);
        Ok(())
    })
}

/// Whether btree node pointers @l and @r are the same, a btree_ptr_v2's
/// mem_ptr aside: btree_bkey_and_val_eq().
fn node_key_eq(l: BkeySC<'_>, r: BkeySC<'_>) -> bool {
    let (a, b) = (l.k, r.k);
    let skip = if a.key_type() == c::bch_bkey_type::KEY_TYPE_btree_ptr_v2 {
        core::mem::offset_of!(c::BtreePtrV2, repr.seq)
    } else {
        0
    };
    a.u64s == b.u64s && a.key_type() == b.key_type() && a.p == b.p && plain_bytes(&a.bversion) == plain_bytes(&b.bversion) &&
        a.size == b.size && l.val_bytes().get(skip..) == r.val_bytes().get(skip..)
}

/// # Safety
/// C has an attempt in progress on @trans, @iter one of its iterators.
#[c_export]
pub unsafe fn bch2_extent_reconcile_pending_mod(trans: &BtreeTrans<'_>, iter: &mut BtreeIter<'_>, level: c_uint,
                                                k: BkeySC<'_>, set: bool) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(pending_mod(&t, iter, level, k, set))
}
