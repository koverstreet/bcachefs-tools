// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::{BkeyI, BkeySC, BKEY_U64S};
use crate::btree::bkey::prelude::*;
use crate::btree::iter::{TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::btree::journal_overlay;
use crate::btree::path::Paths;
use crate::btree::update::peek_slot_exact;
use crate::c;
use crate::data_structures::dst_room::DstRoom;
use crate::errcode::BchError;
use crate::format::{Bpos, Btree, BkeyType};
use crate::prt;
use crate::util::Printbuf;
use core::marker::PhantomData;

/// What a key type's trigger is called with. @new is mutable: an atomic
/// trigger can edit the key going in, in place.
///
/// XXX: TriggerOp ought to be generic over its key type V, @new a V::Mut -
/// grown to MutBkey::MIN_BYTES before any trigger runs, as
/// bkey_make_mut_noupdate()'s copy is, the fields an older value lacks
/// zeroed - so a trigger only ever edits fields in place. Today one can grow the key:
/// bch2_trigger_get_mutable_new() moves the queued update to a bigger buffer
/// mid-commit, and refuses in the atomic phase. That isn't necessarily safe,
/// and a trigger shouldn't have to.
pub struct TriggerOp<'a, 't> {
    pub btree: Btree,
    pub level: u32,
    pub old:   BkeySC<'a>,
    /// The key going in, and its room.
    pub new:   TransBkey<'a, 't>,
    pub flags: UpdateTriggerFlags,
}

impl<'a, 't> TriggerOp<'a, 't> {
    /// @old overwritten by @new, at @level of @btree.
    pub(crate) fn new(btree: Btree, level: u32, old: BkeySC<'a>, new: TransBkey<'a, 't>,
                      flags: UpdateTriggerFlags) -> Self {
        TriggerOp { btree, level, old, new, flags }
    }

    /// @new with room for @needed_u64s, to edit: in place, if it has the
    /// room. An insert's.
    pub fn new_with_room(&mut self, t: &TransAttempt<'_, '_>, needed_u64s: u32)
        -> Result<TransBkey<'_, 't>, BchError>
    {
        assert!(self.flags.contains(UpdateTriggerFlags::INSERT), "growing the key of a trigger that isn't an insert's");

        if needed_u64s > self.new.buf_u64s() {
            self.grow(t, needed_u64s)?;
        }
        Ok(self.new.reborrow())
    }

    /// The queued update moved to a buffer with @needed_u64s of room -
    /// bch2_trigger_get_mutable_new() - and @new repointed there. Not in the
    /// atomic phase.
    #[cold]
    #[inline(never)]
    fn grow(&mut self, t: &TransAttempt<'_, '_>, needed_u64s: u32) -> Result<(), BchError> {
        assert!(!self.flags.contains(UpdateTriggerFlags::ATOMIC), "growing a trigger's key in the atomic phase");

        let k = t.update_grow(self.btree, self.level, self.new.k().p, needed_u64s)?;
        // SAFETY: the update's key now, @needed_u64s long, the trigger's to
        // edit while the attempt runs
        self.new = unsafe { TransBkey::from_raw_room(t, k, needed_u64s) };
        Ok(())
    }

    /// Run @f on the key going out, as an overwrite, then on the key coming
    /// in, as an insert - each that isn't a deletion: as C's
    /// trigger_run_overwrite_then_insert().
    pub fn run_overwrite_then_insert<F>(&self, mut f: F) -> Result<(), BchError>
    where
        F: FnMut(BkeySC<'_>, UpdateTriggerFlags) -> Result<(), BchError>,
    {
        if !self.old.k.key_type().is_deleted() {
            f(self.old, self.flags - UpdateTriggerFlags::INSERT)?;
        }
        if !self.new.k().key_type().is_deleted() {
            f(BkeySC::from(&self.new).untyped(), self.flags - UpdateTriggerFlags::OVERWRITE)?;
        }
        Ok(())
    }
}

/// A key type's trigger - a static: types share a trigger, and an update from
/// one to another of them runs it once, with both keys, so it's compared by
/// address, which a static's is unique to it and a fn's isn't.
pub struct Trigger(pub fn(&TransAttempt<'_, '_>, &mut TriggerOp<'_, '_>) -> Result<(), BchError>);

/// The same trigger - or none, both.
fn same_trigger(a: Option<&'static Trigger>, b: Option<&'static Trigger>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => core::ptr::eq(a, b),
        (a, b)             => a.is_none() && b.is_none(),
    }
}

fn run_trigger(t: &TransAttempt<'_, '_>, trigger: Option<&'static Trigger>, op: &mut TriggerOp<'_, '_>)
    -> Result<(), BchError>
{
    trigger.map_or(Ok(()), |trigger| (trigger.0)(t, op))
}

/// bch2_key_trigger(): @op's key's trigger - the old key's type's, the new
/// key's if it's a deletion - then its snapshot's count of keys.
pub(crate) fn key_trigger(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_, '_>) -> Result<(), BchError> {
    let ty = match op.old.k.key_type() {
        BkeyType::deleted => op.new.k().key_type(),
        ty => ty,
    };
    run_trigger(t, ty.ops().trigger, op)?;

    /*
     * Gate matches the trans-commit dispatch: only snapshotted btrees get
     * per-snapshot key counts. Snapshot-field btrees (reconcile_pending)
     * carry a snapshot in their bpos but must not be counted.
     */
    if op.btree.has_snapshots() {
        crate::alloc::buckets::trigger_snapshot_nr_keys(t, op)?;
    }
    Ok(())
}

/// verify_update_old_key(): @i's old key is the one at its position - the
/// btree's, or before replay's done, the journal's. Debug builds only.
pub(crate) fn verify_update_old_key(t: &TransAttempt<'_, '_>, paths: &Paths<'_>, i: &c::btree_insert_entry) {
    if !cfg!(CONFIG_BCACHEFS_DEBUG) || i.bits.key_cache_flushing() {
        return;
    }

    let mut u = c::bkey::new();
    let mut v = peek_slot_exact(&paths[i.path], &mut u);
    let mut k = &u;

    if let Some(jk) = t.journal_keys() {
        if let Some(j) = journal_overlay::peek_slot(jk, i.bits.btree_id(), i.bits.level().into(), i.key().k.p) {
            k = &j.k;
            v = (&raw const j.v).cast();
        }
    }

    // when updating btree ptrs, mem_ptr may change underneath us, unlocked
    if !k.fields_eq(&i.old_k) || v != i.old_v {
        let mut out = Printbuf::new();
        prt!(&mut out, "updated cached old key doesn't match\ncached: ");
        crate::btree::bkey::bkey_to_text(&mut out, &i.old_k);
        prt!(&mut out, " {:p}\nreal:   ", i.old_v);
        crate::btree::bkey::bkey_to_text(&mut out, k);
        prt!(&mut out, " {:p}", v);
        panic!("{out}");
    }
}

/// @i's update, as its triggers take it: @old_k is the entry's old key, copied
/// out - a trigger can queue updates, and move the entry.
///
/// # Safety
/// @i is queued: its key is the triggers' to edit while the commit runs.
unsafe fn update_op<'a, 't>(t: &TransAttempt<'_, 't>, i: &c::btree_insert_entry, old_k: &'a c::bkey,
                            flags: UpdateTriggerFlags) -> TriggerOp<'a, 't> {
    TriggerOp {
        btree: i.bits.btree_id(),
        level: i.bits.level().into(),
        old:   BkeySC { k: old_k, v: i.old_v, iter: PhantomData, val: PhantomData },
        new:   unsafe { TransBkey::from_raw_room(t, i.key_ptr(), i.key_buf_u64s()) },
        flags,
    }
}

/// A deletion at @p: an op's new key, where it has none.
pub(crate) fn deleted_room(p: Bpos) -> DstRoom<BkeyI, [u64; BKEY_U64S]> {
    let mut room = DstRoom::<BkeyI, [u64; BKEY_U64S]>::zeroed();
    let k = room.head_mut_rest().0;
    *k = c::bkey::new();
    k.p = p;
    room
}

/// run_one_mem_trigger(): @i's triggers in the atomic phase, or gc's, with
/// @flags - its snapshot's count of keys first, from its old key to its new,
/// then its key type's trigger, once, with both keys, where the old and new
/// types share one.
fn run_one_mem_trigger(t: &TransAttempt<'_, '_>, paths: &Paths<'_>, i: &c::btree_insert_entry,
                       flags: UpdateTriggerFlags) -> Result<(), BchError>
{
    verify_update_old_key(t, paths, i);

    let p = i.key().k.p;
    let (no_old, mut no_new) = (BkeyI::<c::Deleted>::at(p), deleted_room(p));
    let old_k = i.old_k;
    let mut op = unsafe { update_op(t, i, &old_k, flags) };
    let old = op.old;
    let (old_ops, new_ops) = (old.k.key_type().ops(), op.new.k().key_type().ops());

    // a net count, old to new: before the split below rewrites @op
    if op.btree.has_snapshots() {
        crate::alloc::buckets::trigger_snapshot_nr_keys(t, &op)?;
    }

    if old_ops.trigger.is_some() && same_trigger(old_ops.trigger, new_ops.trigger) {
        op.flags = flags | UpdateTriggerFlags::INSERT | UpdateTriggerFlags::OVERWRITE;
        return run_trigger(t, old_ops.trigger, &mut op);
    }

    if new_ops.trigger.is_some() {
        op.flags = flags | UpdateTriggerFlags::INSERT;
        op.old   = BkeySC::from(no_old.as_ref());
        run_trigger(t, new_ops.trigger, &mut op)?;
    }

    if old_ops.trigger.is_some() {
        op.flags = flags | UpdateTriggerFlags::OVERWRITE;
        op.old   = old;
        op.new   = TransBkey::from_room(&mut no_new);
        run_trigger(t, old_ops.trigger, &mut op)?;
    }
    Ok(())
}

/// run_one_trans_trigger(): the next of @i's transactional triggers not run -
/// both at once where the old and new types share one, else the overwrite,
/// then the insert - then its snapshot's count of keys. A trigger can queue
/// more updates, so it's handed copies, and the path table's lent across it.
fn run_one_trans_trigger(t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, idx: usize) -> Result<(), BchError> {
    verify_update_old_key(t, paths, &paths.updates()[idx]);

    let i = paths.update_mut(idx);
    let mut deleted = deleted_room(i.key().k.p);
    let old_k = i.old_k;
    let mut flags = UpdateTriggerFlags::from_bits_retain(i.flags) | UpdateTriggerFlags::TRANSACTIONAL;
    let mut op = unsafe { update_op(t, i, &old_k, flags) };
    let (old_ops, new_ops) = (old_k.key_type().ops(), op.new.k().key_type().ops());

    let mut trigger = old_ops.trigger;
    if !i.bits.insert_trigger_run() && !i.bits.overwrite_trigger_run() &&
       same_trigger(old_ops.trigger, new_ops.trigger) {
        i.bits.set_overwrite_trigger_run(true);
        i.bits.set_insert_trigger_run(true);
        flags |= UpdateTriggerFlags::INSERT | UpdateTriggerFlags::OVERWRITE;
    } else if !i.bits.overwrite_trigger_run() {
        i.bits.set_overwrite_trigger_run(true);
        flags |= UpdateTriggerFlags::OVERWRITE;
        op.new = TransBkey::from_room(&mut deleted);
    } else if !i.bits.insert_trigger_run() {
        i.bits.set_insert_trigger_run(true);
        flags |= UpdateTriggerFlags::INSERT;
        op.old = BkeySC::from(&*deleted);
        trigger = new_ops.trigger;
    }
    op.flags = flags;

    paths.lend(|| {
        run_trigger(t, trigger, &mut op)?;
        if op.btree.has_snapshots() {
            crate::alloc::buckets::trigger_snapshot_nr_keys(t, &op)?;
        }
        Ok(())
    })
}

fn trans_triggers_run(i: &c::btree_insert_entry) -> bool {
    i.flags & UpdateTriggerFlags::NORUN.bits() != 0 ||
        !i.has_trans_triggers() ||
        (i.bits.insert_trigger_run() && i.bits.overwrite_trigger_run())
}

/// The commit's transactional triggers - its updates', and those of the
/// updates they queue.
pub(crate) fn run_trans_triggers(t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>) -> Result<(), BchError> {
    let mut sort_id_start = 0;

    while sort_id_start < paths.updates().len() {
        let sort_id = paths.updates()[sort_id_start].sort_order;
        let mut idx;

        /*
         * For a given btree, this algorithm runs insert triggers before
         * overwrite triggers: this is so that when extents are being
         * moved (e.g. by FALLOCATE_FL_INSERT_RANGE), we don't drop
         * references before they are re-added.
         *
         * Running triggers will append more updates to the list of
         * updates as we're walking it:
         */
        loop {
            let mut trans_trigger_run = false;

            idx = sort_id_start;
            while idx < paths.updates().len() {
                /*
                 * By index each time, as C's loops do: a trigger queues more,
                 * and a path it allocates can move them all, to a bigger array.
                 */
                let i = &paths.updates()[idx];
                let (sort_order, run) = (i.sort_order, trans_triggers_run(i));
                if sort_order > sort_id {
                    break;
                }

                if sort_order < sort_id {
                    sort_id_start = idx;
                } else if !run {
                    run_one_trans_trigger(t, paths, idx)?;
                    trans_trigger_run = true;
                }
                idx += 1;
            }

            if !trans_trigger_run {
                break;
            }
        }

        sort_id_start = idx;
    }

    if cfg!(CONFIG_BCACHEFS_DEBUG) {
        let updates = paths.updates();
        for (idx, i) in updates.iter().enumerate() {
            assert!(trans_triggers_run(i), "update {idx} of {}: transactional triggers not run", updates.len());
        }
    }
    Ok(())
}

/// The commit's atomic triggers, then gc's, if it's running: those for the
/// updates its walk has passed. In the atomic phase, which has @paths.
pub(crate) fn run_mem_triggers(t: &TransAttempt<'_, '_>, paths: &Paths<'_>) -> Result<(), BchError> {
    for i in paths.updates() {
        let flags = UpdateTriggerFlags::from_bits_retain(i.flags);

        if i.has_atomic_triggers() && !flags.contains(UpdateTriggerFlags::NORUN) {
            run_one_mem_trigger(t, paths, i, flags | UpdateTriggerFlags::ATOMIC)?;
        }
    }

    if t.fs().gc.phase() != c::GcPhase::not_running {
        run_gc_triggers(t, paths)?;
    }
    Ok(())
}

#[cold]
fn run_gc_triggers(t: &TransAttempt<'_, '_>, paths: &Paths<'_>) -> Result<(), BchError> {
    let gc = &t.fs().gc;

    for i in paths.updates() {
        let flags = UpdateTriggerFlags::from_bits_retain(i.flags);

        if i.has_triggers() && !flags.contains(UpdateTriggerFlags::NORUN) &&
           gc.visited(c::gc_pos::in_btree(i.bits.btree_id(), i.bits.level().into(), i.key().k.p)) {
            run_one_mem_trigger(t, paths, i, flags | UpdateTriggerFlags::GC)?;
        }
    }
    Ok(())
}
