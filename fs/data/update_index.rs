// SPDX-License-Identifier: GPL-2.0

//! The data update path's index update: once a data update's write is done,
//! the replicas it wrote go into the extent, in place of the ones it was
//! moving. Setting the update up, its read and its teardown are in
//! update.rs.
//!
//! An update is of @old, the key as it was when the update was set up. The
//! btree may have changed since - the extent cut or overwritten, pointers added
//! or dropped by other movers - so a key now there that isn't @old's data
//! (extents_match()) is skipped, and the update's masks, of @old's pointers,
//! are remapped onto the key that is (PtrMask::remap()).
//!
//! Per key there are two copies in transaction memory: @new, what the write
//! wrote, and @insert, what's there, both cut to where they overlap. @insert's
//! conflicting replicas are dropped, @new's pointers go in - each under its
//! crc, so ahead of pointers already there unless it brings a crc of its own -
//! and what isn't needed is dropped or made cached. Each of those renumbers
//! @insert's pointers and ends its brand, so every mask is remapped afresh for
//! the next. Both copies have room for what set_needs_reconcile() adds.
//!
//! Each key is an attempt of the transaction's walk, committed inside it, as
//! C's: the trace and where the walk goes on from come after the commit.
//!
//! Durability is checked twice. Before the drops, a key that would end up less
//! durable than it was, and than the options want, is skipped - a race with
//! erasure coding. After, with the options unchanged since the write started
//! (the change cookie, reread after a barrier), one that would lose durability
//! or have none takes the filesystem emergency read-only: it's a bug.

use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::{bkey_le, BkeyFamily, BkeyI, BkeySC, BkeyV};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts, LoopFlow, TransAttempt,
                         TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::c::bch_errcode;
use crate::data::durability::{drop_extra_durability, drop_extra_ec_durability, BkeyVDurability};
use crate::data::extents::{BkeyVExtents, Extents};
use crate::data::update::{ptr_bits_to_text, TYPE_NAMES};
use crate::data::ptrs::{brand, extents_match, BkeyVPtrs, Branded, PtrMask, Ptrs, PtrsMut};
use crate::data::reconcile::plan::pending_mod;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, id};
use crate::inode_opts::get_io_opts;
use crate::sb::counters::events;
use crate::util::log::LogMsg;
use crate::util::{Printbuf, ToText};
use crate::{bch_err_fn, log_msg_ratelimited};
use core::ffi::{c_int, c_uint};
use core::mem::size_of;
use core::sync::atomic::{fence, Ordering};
use cstruct_macros::c_export;

/// Room past a key for set_needs_reconcile(): an entry, and a padding pointer
/// per replica.
const RECONCILE_ROOM: usize = (1 + c::BCH_REPLICAS_MAX as usize) * size_of::<u64>();

// The update's keys, narrowed to what they're known to be: of an extents
// btree, and once one matched @old, of its type - with pointers. One that
// isn't is a bug, and the panic has the key.
const MATCHED: &str = "matched the update's key but has no pointers";

fn check_is<F: BkeyFamily + ?Sized>(fs: &Fs, k: BkeySC<'_>, what: &str) {
    assert!(F::contains(k.k), "data update: {what}: {}", k.to_text(fs));
}

fn extents_mut<'b, 't>(fs: &Fs, k: &'b mut TransBkey<'_, 't>) -> TransBkey<'b, 't, Extents> {
    check_is::<Extents>(fs, k.as_sc(), "not a key of an extents btree");
    k.reborrow().arm::<Extents>().unwrap()
}

fn ptrs<'b>(fs: &Fs, k: &'b TransBkey<'_, '_>) -> BkeySC<'b, Ptrs> {
    check_is::<Ptrs>(fs, k.as_sc(), MATCHED);
    k.as_sc().arm::<Ptrs>().unwrap()
}

fn ptrs_mut<'b, 't>(fs: &Fs, k: &'b mut TransBkey<'_, 't>) -> TransBkey<'b, 't, PtrsMut> {
    check_is::<PtrsMut>(fs, k.as_sc(), MATCHED);
    k.reborrow().arm::<PtrsMut>().unwrap()
}

/// @f with @insert under a fresh brand, and @bits - a mask of @old's - remapped
/// onto it.
fn with_remapped<'b, 't, R>(fs: &Fs, old: BkeySC<'_, Ptrs>, insert: &'b mut TransBkey<'_, 't>, bits: u16,
                            f: impl for<'i> FnOnce(Branded<'i, TransBkey<'b, 't, PtrsMut>>, PtrMask<'i>) -> R) -> R
{
    brand(old, |o| brand(ptrs_mut(fs, insert), |i| {
        let m = o.remap(fs, o.assume(fs, bits.into()), &i);
        f(i, m)
    }))
}

/// @bits, a mask of @old's pointers, as @new's - none, of a key without them.
fn remap_bits(fs: &Fs, old: BkeySC<'_>, bits: u32, new: BkeySC<'_>) -> u16 {
    let (Some(old), Some(new)) = (old.arm::<Ptrs>(), new.arm::<Ptrs>()) else { return 0 };
    brand(old, |o| brand(new, |n| o.remap(fs, o.assume(fs, bits), &n).bits()))
}

/// For move.c's journal scrub, until it's Rust.
#[c_export]
pub fn ptr_mask_remap(c: &Fs, old: BkeySC<'_>, oldmask: c_uint, new: BkeySC<'_>) -> c_uint {
    remap_bits(c, old, oldmask, new).into()
}

/// Index update keys, for the key fail traces and the messages of an update
/// that went wrong: @new, the key in the btree, @wrote, what the write wrote,
/// cut to it, @insert, what was going in.
#[cold]
fn key_to_text(out: &mut Printbuf, fs: &Fs, u: &c::data_update, new: BkeySC<'_>, wrote: BkeySC<'_>,
               insert: BkeySC<'_>) {
    let old = BkeySC::from(u.k.key());

    write!(out, "rewrites found:\t{:b}", remap_bits(fs, old, u.opts.ptrs_kill.into(), insert));
    out.newline();

    fs.data_update_opts_to_text(out, &u.op.opts, &u.opts);

    write!(out, "\nold:    {}", old.to_text(fs));
    write!(out, "\nnew:    {}", new.to_text(fs));
    write!(out, "\nwrote:  {}", wrote.to_text(fs));
    write!(out, "\ninsert: {}", insert.to_text(fs));
}

/// A key the update skipped, counted in its stats and traced.
#[cold]
#[inline(never)]
fn count_key_fail(fs: &Fs, u: &c::data_update, new: BkeySC<'_>, wrote: BkeySC<'_>, insert: BkeySC<'_>, msg: &str) {
    if let Some(stats) = &u.stats {
        stats.keys_raced.fetch_add(1, Ordering::Relaxed);
        stats.sectors_raced.fetch_add(insert.k.size.into(), Ordering::Relaxed);
    }

    fs.event_add_trace(events::data_update_key_fail, insert.k.size.into(), |out| {
        write!(out, "{msg}\n");
        key_to_text(out, fs, u, new, wrote, insert);
    });
}

/// The update of @k, the slot at @iter, by the front of @u's keylist; where
/// the walk goes on from.
fn index_update_key<'t>(t: &TransAttempt<'_, 't>, u: &c::data_update, iter: &BtreeIter<'t>, k: BkeySC<'_>)
    -> Result<c::bpos, BchError>
{
    let fs = t.fs();
    let old = BkeySC::from(u.k.key());

    /* make a local copy, so that we can trace it after the transaction commit: */
    let k = t.bkey_make_mut_noupdate(k)?;

    let front = BkeySC::from(u.op.insert_keys.front().expect("an index update of an empty keylist"));
    let mut new: TransBkey<'_, '_> = t.bkey_reassemble_room(front, front.k.bytes() + RECONCILE_ROOM)?;
    // and room for @new's pointers
    let mut insert: TransBkey<'_, '_> =
        t.bkey_reassemble_room(k.as_sc(), k.k().bytes() + front.k.val_bytes() + RECONCILE_ROOM)?;

    // None of the cuts empties a key: the walk pops each key ending at or
    // before where it goes on, so @new ends past @pos, and the slot there does
    let pos = iter.pos();
    extents_mut(fs, &mut new).cut_front_in_place(fs, pos);
    extents_mut(fs, &mut insert).cut_front_in_place(fs, pos);
    let end = new.k().p;
    extents_mut(fs, &mut insert).cut_back_in_place(end);
    let end = insert.k().p;
    extents_mut(fs, &mut new).cut_back_in_place(end);

    let next_pos = insert.k().p;

    if !extents_match(fs, k.as_sc(), old) {
        count_key_fail(fs, u, k.as_sc(), new.as_sc(), insert.as_sc(), "no match:");
        return Ok(next_pos);
    }

    let old = old.arm::<Ptrs>().unwrap_or_else(|| panic!("data update of a key without pointers: {}", old.to_text(fs)));

    /* Don't replace non-cached replicas with cached: */
    let replacing_non_cached = with_remapped(fs, old, &mut insert, u.opts.ptrs_kill,
                                             |i, kill| i.ptrs_in(fs, kill).any(|p| p.cached() == 0));

    if replacing_non_cached && ptrs(fs, &new).ptrs(fs).any(|p| p.cached() != 0) {
        let mut msg = log_msg_ratelimited!(fs);
        write!(msg, "data update tried to replace non cached data with cached:\n");
        key_to_text(&mut msg, fs, u, k.as_sc(), new.as_sc(), insert.as_sc());
        return Ok(next_pos);
    }

    if !u.opts.bits.no_devs_have() {
        /*
         * Drop replicas in the existing extent that conflict with what we just
         * wrote - but only if they were explicitly specified in ptrs_kill: a
         * replica that we just wrote might conflict with a replica that we
         * want to keep, due to racing with another move:
         */
        let new_p = ptrs(fs, &new);
        with_remapped(fs, old, &mut insert, u.opts.ptrs_kill, |i, kill| {
            let conflicts = i.on_devs_of(fs, &new_p, |_| true) & kill;
            i.drop_ptrs_mask(fs, conflicts);
        });

        /*
         * Any conflicts that are left over were useless writes - perhaps due
         * to a copygc race. But a new durable pointer doesn't conflict with a
         * cached pointer on the same device - it replaces it:
         */
        let insert_p = ptrs(fs, &insert);
        let conflicted = brand(ptrs_mut(fs, &mut new), |n| {
            let conflicts = n.on_devs_of(fs, &insert_p, |p| p.cached() == 0);
            if !conflicts.is_empty() {
                fs.event_add_trace(events::data_update_useless_write_fail,
                                   u64::from(k.k().size) * u64::from(conflicts.bits().count_ones()), |out| {
                    ptr_bits_to_text(out, conflicts.bits(), "conflicted");
                    write!(out, "wrote: {}", n.key().to_text(fs));
                    out.newline();
                    u.to_text(out);
                });
                n.drop_ptrs_mask(fs, conflicts);
            }
            !conflicts.is_empty()
        });

        if conflicted && new.as_sc().val_bytes().is_empty() {
            count_key_fail(fs, u, k.as_sc(), front, insert.as_sc(), "new replicas conflicted:");
            return Ok(k.k().p);
        }
    }

    /*
     * Drop all conflicts from the existing extent, not the newly written
     * replicas:
     *
     * When converting an extent to erasure coding, we don't disallow the
     * write from allocating on the extent's existing devices: we just want a
     * new replica that will have a stripe pointer added asynchronously by
     * erasure coding, and it can overwrite whichever device it happens to
     * land on
     *
     * XXX: make sure drop_extra_replicas does not drop the new replica
     */
    let new_p = ptrs(fs, &new);
    brand(ptrs_mut(fs, &mut insert), |i| {
        let conflicts = i.on_devs_of(fs, &new_p, |_| true);
        i.drop_ptrs_mask(fs, conflicts);
    });

    let mut i = ptrs_mut(fs, &mut insert);
    for p in new_p.ptrs_decoded(fs) {
        i.insert_decoded_ptr(fs, &p.p).unwrap_or_else(|e| {
            panic!("data update: {e}, adding the pointers of {} to {}", new_p.to_text(fs), i.to_text(fs))
        });
    }

    let opts = get_io_opts(t, None, k.as_sc())?;

    let old_durability = k.as_sc().durability(t)?;
    let new_durability = insert.as_sc().durability(t)?;
    let want = u.op.opts.data_replicas.min(opts.data_replicas);

    if new_durability.total < old_durability.total && new_durability.total < want {
        /*
         * This can happen when a move - evacuate or copygc - races with an
         * extent being erasure coded: we replaced a pointer with one on the
         * same device (so we have to drop it early, to avoid conflicts) which
         * is no longer erasure coded.
         *
         * XXX: trace this
         */
        return Ok(next_pos);
    }

    with_remapped(fs, old, &mut insert, u.opts.ptrs_io_error, |i, m| drop_extra_durability(t, &opts, i, m, true))?;
    with_remapped(fs, old, &mut insert, u.opts.ptrs_kill_ec, |mut i, m| drop_extra_ec_durability(t, &opts, &mut i, m))?;
    with_remapped(fs, old, &mut insert, u.opts.ptrs_kill, |i, m| drop_extra_durability(t, &opts, i, m, false))?;

    /* Prefer to drop existing replicas over new - we don't want to drop the
     * new replica being erasure coded*/
    // XXX (C's): else prefer to drop new replicas, and if we did, trace that we
    // did useless work - break up drop_extra_durability() so we can trace what
    // we're doing (or have it return a mask)
    if u.opts.bits.no_devs_have() {
        brand(ptrs_mut(fs, &mut insert), |i| {
            let m = i.except(fs, i.on_devs_of(fs, &new_p, |_| true));
            drop_extra_durability(t, &opts, i, m, false)
        })?;
    }

    brand(ptrs_mut(fs, &mut insert), |i| { let all = i.all(fs); drop_extra_durability(t, &opts, i, all, false) })?;

    let mut i = ptrs_mut(fs, &mut insert);
    i.drop_extra_cached_ptrs(fs, &opts);
    i.propagate_incompressible(fs, k.as_sc());

    let s = t.sum_sector_overwrites(iter, insert.as_ref())?;

    let res = &u.op.res;
    if s.disk_sectors_delta > res.sectors() as i64 {
        res.add_physical((s.disk_sectors_delta - res.sectors() as i64) as u64, s.new_nr_replicas,
                         if s.usage_increasing { c::bch_reservation_flags::empty() }
                         else { c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL })?;
    }

    t.log_str(TYPE_NAMES[u.opts.type_ as usize].to_bytes())?;
    t.log_bkey(u.btree_id, 0, u.k.key())?;

    /* needs k.k->p, the whole key's end - see index_update(): */
    t.insert_snapshot_whiteouts(u.btree_id, k.k().p, insert.k().p)?;

    /*
     * This set_needs_reconcile call is only for verifying that the data we
     * just wrote was written correctly, otherwise we could fail to flag
     * incorrectly written data due to needs_rb already being set on the
     * existing extent
     */
    new.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_foreground,
                            u.op.opts.change_cookie)?;

    if u.op.opts.change_cookie == fs.opt_change_cookie.get() {
        let old_durability = k.as_sc().durability(t)?;
        let new_durability = insert.as_sc().durability(t)?;

        if ((new_durability.total < old_durability.total && new_durability.total < want) ||
            new_durability.total == 0) &&
            {
                fence(Ordering::SeqCst);
                u.op.opts.change_cookie == fs.opt_change_cookie.get()
            }
        {
            let mut msg = LogMsg::new(fs);
            write!(msg, "Data update would have reduced extent durability:\n");
            write!(msg, "Old extent {}, new {}, option specifies {}\n",
                   old_durability.total, new_durability.total, opts.data_replicas);
            key_to_text(&mut msg, fs, u, k.as_sc(), new.as_sc(), insert.as_sc());
            fs.emergency_read_only(&mut msg);
            return fs.throw(bch_errcode::BCH_ERR_emergency_ro);
        }
    }

    /* This is the real set_needs_reconcile() call */
    insert.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_foreground,
                               u.op.opts.change_cookie)?;

    t.update(iter, &insert, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE)?;
    t.commit(Some(res), CommitOpts::from(u.opts.commit_flags)
                        .flags(CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC))?;

    fs.event_add_trace(events::data_update_key, new.k().size.into(), |out| {
        write!(out, "\nold: {}", old.to_text(fs));
        write!(out, "\nk:   {}", k.as_sc().to_text(fs));
        write!(out, "\nnew: {}", insert.as_sc().to_text(fs));
    });

    Ok(next_pos)
}

/// Whether @insert, the keylist's front, is an extent - all a data update's
/// write writes - logged and counted as an fsck error if it isn't.
fn insert_is_extent(fs: &Fs, insert: &BkeyI) -> bool {
    if insert.k().key_type() == c::bch_bkey_type::KEY_TYPE_extent {
        return true;
    }

    let mut msg = LogMsg::new(fs);
    write!(msg, "Got non-extent key to insert in data update path - confused:\n{}",
           BkeySC::from(insert).to_text(fs));
    let counted = count_fsck_err(fs, id::data_update_got_non_extent, &mut msg);
    msg.set_suppressed(!counted);
    false
}

pub(crate) fn index_update(trans: &BtreeTrans<'_>, u: &mut c::data_update) -> Result<(), BchError> {
    let fs = trans.fs();
    let Some(front) = u.op.insert_keys.front().filter(|k| insert_is_extent(fs, k)) else { return Ok(()) };

    /*
     * Slots, not BTREE_ITER_trim: each key must be whole. Where @insert ends
     * before @k, the rewritten part is a key at a new position, insert->k.p,
     * and descendant snapshots' overwrites of @k, at k.k->p, don't hide it
     * there: insert_snapshot_whiteouts() adds whiteouts for them by comparing
     * the two ends - which a trimmed @k would make equal.
     */
    let mut iter = BtreeIter::new(trans, u.btree_id, front.k().start_pos(),
                                  BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT);

    iter.for_each(trans, |t, iter, k| {
        let next = index_update_key(t, u, iter, k)?;

        while u.op.insert_keys.front().is_some_and(|k| bkey_le(k.k().p, next)) {
            u.op.insert_keys.pop_front();
        }

        // the next key's checked before the walk peeks for it, as C's loop
        Ok(match u.op.insert_keys.front() {
            Some(front) if insert_is_extent(fs, front) => LoopFlow::ContinueFrom(next),
            _ => LoopFlow::Break(()),
        })
    })
}

/// @u's index update, in a transaction of its own: the write path's, once the
/// write op is done.
#[c_export]
pub fn bch2_data_update_index_update(u: &mut c::data_update) -> c_int {
    let trans = crate::btree_trans!(u.op.fs());
    ret_to_c(index_update(&trans, u))
}

/// @f on each slot @u's extent covered, an attempt and a commit each.
fn for_each_old_slot<'t, F>(trans: &'t BtreeTrans<'t>, u: &c::data_update, res: Option<&DiskReservation<'_>>,
                            mut f: F) -> Result<(), BchError>
where
    F: for<'a, 'k> FnMut(&'k TransAttempt<'a, 't>, &'k BtreeIter<'t>, BkeySC<'k>) -> Result<(), BchError>,
{
    let old = BkeySC::from(u.k.key());
    let mut iter = BtreeIter::new(trans, u.btree_id, old.k.start_pos(), BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT);
    iter.for_each_max_commit(trans, old.k.p, BtreeIterFlags::SLOTS, res,
                             CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC, |t, iter, k| {
        // the slot at @old's end: where C's loop breaks
        if bkey_le(old.k.p, k.k.start_pos()) {
            return Ok(());
        }
        f(t, iter, k)
    })
}

/// The index update for a data update whose write didn't happen, because the
/// read failed on every replica: drops its ptrs_kill, and logs it.
pub(crate) fn index_update_nowrite(trans: &BtreeTrans<'_>, u: &c::data_update) -> Result<(), BchError> {
    let fs = trans.fs();
    let old = BkeySC::from(u.k.key());
    let old_p = old.arm::<Ptrs>().unwrap_or_else(|| panic!("data update of a key without pointers: {}", old.to_text(fs)));

    let mut msg = LogMsg::new(fs);
    write!(msg, "data_update_index_update_nowrite():\n");
    write!(msg, "old: {}", old.to_text(fs));
    msg.newline();
    ptr_bits_to_text(&mut msg, u.opts.ptrs_kill, "ptrs_kill");

    let res = DiskReservation::new(fs);

    assert_eq!(u.opts.ptrs_kill_ec, 0, "a nowrite data update dropping stripe pointers");
    assert_ne!(u.opts.ptrs_kill, 0, "a nowrite data update with nothing to drop");

    for_each_old_slot(trans, u, Some(&res), |t, iter, k| {
        write!(msg, "k: {}", k.to_text(fs));
        msg.newline();

        let Some(kp) = k.arm::<Ptrs>().filter(|_| extents_match(fs, k, old)) else { return Ok(()) };
        let Some(new) = brand(old_p, |o| brand(kp, |kb| -> Result<_, BchError> {
            let kill = o.remap(fs, o.assume(fs, u.opts.ptrs_kill.into()), &kb);
            if kill.is_empty() {
                return Ok(None);
            }
            let new = kb.adopt(fs, t.bkey_reassemble_room(kb.key(), k.k.bytes() + RECONCILE_ROOM)?);
            Ok(Some(new.drop_ptrs_mask(fs, kill)))
        }))? else {
            return Ok(());
        };
        let mut new: TransBkey<'_, '_> = new.into();

        let opts = get_io_opts(t, None, k)?;
        new.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_foreground,
                                u.op.opts.change_cookie.wrapping_sub(1))?;
        t.update(iter, &new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE)?;

        write!(msg, "new: {}", new.as_sc().to_text(fs));
        msg.newline();
        Ok(())
    })
}

/// EC allocation failed - the write never happened, but the extent still
/// needs erasure coding. Mark it pending so reconcile retries from the pending
/// list instead of the main scan.
pub(crate) fn ec_alloc_failed(u: &mut c::data_update) {
    let fs = u.op.fs();
    let trans = crate::btree_trans!(fs);
    let old = BkeySC::from(u.k.key());
    let mut parked = false;

    let ret = bch_err_fn!(fs, for_each_old_slot(&trans, u, None, |t, iter, k| {
        if !extents_match(fs, k, old) {
            return Ok(());
        }

        fs.event_add_trace(events::reconcile_set_pending, k.k.size.into(), |out| {
            write!(out, "{}\n{}\n", BchError::from_raw(-i32::from(u.op.error)), k.to_text(fs));
            fs.data_update_opts_to_text(out, &u.op.opts, &u.opts);
        });

        pending_mod(t, iter, 0, k, true)?;
        parked = true;
        Ok(())
    }));

    /*
     * Nothing matched: the extent we were reconciling was rewritten or
     * deleted while our write was in flight, so there was nothing to park.
     * Distinct error code so the update's exit() still traces it -
     * ec_alloc_failed itself is suppressed there because it parks.
     */
    if ret.is_ok() && !parked {
        u.op.error = -fs.err(bch_errcode::BCH_ERR_ec_alloc_failed_pending_race).raw() as i16;
    }
}
