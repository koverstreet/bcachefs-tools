// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::{BkeyI, BkeyS, BkeySC};
use crate::btree::bkey::prelude::*;
use crate::btree::iter::{BtreeTrans, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::format::{Bpos, Btree, BkeyType};
use crate::prt;
use crate::util::Printbuf;
use core::ffi::c_int;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use cstruct_macros::c_export;

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
pub struct TriggerOp<'a> {
    pub btree: Btree,
    pub level: u32,
    pub old:   BkeySC<'a>,
    pub new:   BkeyS<'a>,
    pub flags: UpdateTriggerFlags,
    /// The room @new has - for a C trigger it's handed back to
    new_buf_u64s: u32,
}

impl<'a> TriggerOp<'a> {
    /// # Safety
    /// @op is the one the trigger was called with: both keys valid, and @new
    /// the trigger's to edit, for 'a.
    pub unsafe fn from_raw(op: &'a mut c::btree_trigger_op) -> Self {
        let new = unsafe { &mut op.new.split.kv };
        TriggerOp {
            btree: op.btree,
            level: op.level,
            old:   unsafe { BkeySC::from_c(&op.old) },
            new:   BkeyS { k: unsafe { &mut *new.k }, v: new.v, val: PhantomData },
            flags: UpdateTriggerFlags::from_bits_retain(op.flags.bits()),
            new_buf_u64s: op.new_buf_u64s,
        }
    }

    /// As C's btree_trigger_op, for a C trigger this one runs: @new lent to it
    /// for as long as this is borrowed.
    pub(crate) fn to_raw(&mut self) -> c::btree_trigger_op {
        c::btree_trigger_op {
            btree:        self.btree,
            level:        self.level,
            old:          self.old.into(),
            new:          self.new.as_raw(),
            new_buf_u64s: self.new_buf_u64s,
            flags:        c::btree_iter_update_trigger_flags::from_bits_retain(self.flags.bits()),
        }
    }

    /// @new with room for @needed_u64s, to edit: the queued update moved to a
    /// bigger buffer if it hasn't the room - bch2_trigger_get_mutable_new() -
    /// and @new repointed there. An insert's, outside the atomic phase.
    ///
    /// The key borrows the op: it can be @new itself.
    pub fn new_with_room<'s, 't>(&'s mut self, t: &TransAttempt<'s, 't>, needed_u64s: u32)
        -> Result<TransBkey<'s, 't>, BchError>
    {
        let mut out = MaybeUninit::<c::bkey_s>::uninit();
        // SAFETY: the op the trigger was called with, in its commit
        t.result(unsafe { c::bch2_trigger_get_mutable_new(t.raw(), self.to_raw(), needed_u64s, out.as_mut_ptr()) })?;
        // SAFETY: written on success - the update's key, in transaction
        // memory, which outlives the op
        let out = unsafe { out.assume_init().split.kv };
        self.new = BkeyS { k: unsafe { &mut *out.k }, v: out.v, val: PhantomData };
        self.new_buf_u64s = self.new_buf_u64s.max(needed_u64s);

        // SAFETY: @new, @new_buf_u64s long, the trigger's to edit while the
        // attempt runs
        Ok(unsafe { TransBkey::from_raw_room(t, core::ptr::from_mut(self.new.k).cast(), self.new_buf_u64s) })
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
        if !self.new.k.key_type().is_deleted() {
            f(BkeySC::from(&self.new).untyped(), self.flags - UpdateTriggerFlags::OVERWRITE)?;
        }
        Ok(())
    }
}

/// A key type's trigger - a static: types share a trigger, and an update from
/// one to another of them runs it once, with both keys, so it's compared by
/// address, which a static's is unique to it and a fn's isn't.
pub struct Trigger(pub fn(&TransAttempt<'_, '_>, &mut TriggerOp<'_>) -> Result<(), BchError>);

/// The same trigger - or none, both.
fn same_trigger(a: Option<&'static Trigger>, b: Option<&'static Trigger>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => core::ptr::eq(a, b),
        (a, b)             => a.is_none() && b.is_none(),
    }
}

fn run_trigger(t: &TransAttempt<'_, '_>, trigger: Option<&'static Trigger>, op: &mut TriggerOp<'_>)
    -> Result<(), BchError>
{
    trigger.map_or(Ok(()), |trigger| (trigger.0)(t, op))
}

/// bch2_key_trigger(): @op's key's trigger - the old key's type's, the new
/// key's if it's a deletion - then its snapshot's count of keys.
fn key_trigger(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    let ty = match op.old.k.key_type() {
        BkeyType::deleted => op.new.k.key_type(),
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

/// # Safety
/// @op is one C runs a trigger for, in its commit's attempt.
#[c_export]
pub unsafe fn bch2_key_trigger(trans: &BtreeTrans<'_>, mut op: c::btree_trigger_op) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(key_trigger(&t, &mut unsafe { TriggerOp::from_raw(&mut op) }))
}

/// verify_update_old_key(): @i's old key is the one at its position - the
/// btree's, or before replay's done, the journal's. Debug builds only.
///
/// # Safety
/// @i is an update of @trans's.
#[c_export]
pub unsafe fn bch2_verify_update_old_key(trans: &BtreeTrans<'_>, i: &c::btree_insert_entry) {
    if !cfg!(CONFIG_BCACHEFS_DEBUG) || i.bits.key_cache_flushing() {
        return;
    }

    let path = unsafe { trans.paths.get().add(i.path as usize) };
    let mut u = c::bkey::new();
    let mut k = unsafe { crate::btree::iter::bch2_btree_path_peek_slot(path, &mut u) };

    // bch2_btree_path_peek_slot() for a cached iterator might return a key in a
    // different snapshot:
    let pos = unsafe { (*path).pos };
    if k.k.is_null() || unsafe { (*k.k).p } != pos {
        u = c::bkey { p: pos, ..c::bkey::new() };
        k = c::bkey_s_c { k: &u, v: core::ptr::null() };
    }

    if trans.journal_replay_not_finished() {
        let j = unsafe { crate::btree::journal_overlay::peek_slot(
            trans.fs(), i.bits.btree_id(), i.bits.level().into(), (*i.k).k.p) };
        if let Some(j) = unsafe { j.as_ref() } {
            k = c::bkey_s_c { k: &j.k, v: &j.v };
        }
    }

    // when updating btree ptrs, mem_ptr may change underneath us, unlocked
    let k_k = unsafe { &*k.k };
    if !k_k.fields_eq(&i.old_k) || k.v != i.old_v {
        let mut out = Printbuf::new();
        prt!(&mut out, "updated cached old key doesn't match\ncached: ");
        crate::btree::bkey::bkey_to_text(&mut out, &i.old_k);
        prt!(&mut out, " {:p}\nreal:   ", i.old_v);
        crate::btree::bkey::bkey_to_text(&mut out, k_k);
        prt!(&mut out, " {:p}", k.v);
        panic!("{out}");
    }
}

/// A deletion at @p, for the side of an update that has no key.
fn deleted_at(p: Bpos) -> c::bkey_i {
    c::bkey_i { k: c::bkey { p, ..c::bkey::new() }, ..Default::default() }
}

fn as_s(k: &mut c::bkey_i) -> BkeyS<'_> {
    BkeyS { k: &mut k.k, v: &mut k.v, val: PhantomData }
}

/// @i's update, as its triggers take it: @old_k is the entry's old key, copied
/// out - a trigger can queue updates, and move the entry.
///
/// # Safety
/// As run_one_mem_trigger().
unsafe fn update_op<'a>(i: &c::btree_insert_entry, old_k: &'a c::bkey, flags: UpdateTriggerFlags) -> TriggerOp<'a> {
    TriggerOp {
        btree:        i.bits.btree_id(),
        level:        i.bits.level().into(),
        old:          BkeySC { k: old_k, v: i.old_v, iter: PhantomData, val: PhantomData },
        new:          BkeyS::from(unsafe { &mut *BkeyI::from_raw_mut(i.k) }),
        flags,
        new_buf_u64s: i.k_buf_u64s.into(),
    }
}

/// run_one_mem_trigger(): @i's triggers in the atomic phase, or gc's, with
/// @flags - its snapshot's count of keys first, from its old key to its new,
/// then its key type's trigger, once, with both keys, where the old and new
/// types share one.
///
/// # Safety
/// @i is an update of @trans's, in its commit.
unsafe fn run_one_mem_trigger(trans: &BtreeTrans<'_>, t: &TransAttempt<'_, '_>, i: &mut c::btree_insert_entry,
                              flags: UpdateTriggerFlags) -> Result<(), BchError>
{
    unsafe { bch2_verify_update_old_key(trans, i) };

    let p = unsafe { (*i.k).k.p };
    let (no_old, mut no_new) = (deleted_at(p), deleted_at(p));
    let old_k = i.old_k;
    let mut op = unsafe { update_op(i, &old_k, flags) };
    let old = op.old;
    let (old_ops, new_ops) = (old.k.key_type().ops(), op.new.k.key_type().ops());

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
        op.old   = BkeySC::from_bkey_i(&no_old);
        run_trigger(t, new_ops.trigger, &mut op)?;
    }

    if old_ops.trigger.is_some() {
        op.flags        = flags | UpdateTriggerFlags::OVERWRITE;
        op.old          = old;
        op.new          = as_s(&mut no_new);
        op.new_buf_u64s = 0;
        run_trigger(t, old_ops.trigger, &mut op)?;
    }
    Ok(())
}

/// run_one_trans_trigger(): the next of @i's transactional triggers not run -
/// both at once where the old and new types share one, else the overwrite,
/// then the insert - then its snapshot's count of keys. A trigger can queue
/// more updates, so it's handed copies, not @i.
///
/// # Safety
/// As run_one_mem_trigger().
unsafe fn run_one_trans_trigger(trans: &BtreeTrans<'_>, t: &TransAttempt<'_, '_>, i: &mut c::btree_insert_entry)
    -> Result<(), BchError>
{
    unsafe { bch2_verify_update_old_key(trans, i) };

    let mut deleted = deleted_at(unsafe { (*i.k).k.p });
    let old_k = i.old_k;
    let mut flags = UpdateTriggerFlags::from_bits_retain(i.flags) | UpdateTriggerFlags::TRANSACTIONAL;
    let mut op = unsafe { update_op(i, &old_k, flags) };
    let (old_ops, new_ops) = (old_k.key_type().ops(), op.new.k.key_type().ops());

    let mut trigger = old_ops.trigger;
    if !i.bits.insert_trigger_run() && !i.bits.overwrite_trigger_run() &&
       same_trigger(old_ops.trigger, new_ops.trigger) {
        i.bits.set_overwrite_trigger_run(true);
        i.bits.set_insert_trigger_run(true);
        flags |= UpdateTriggerFlags::INSERT | UpdateTriggerFlags::OVERWRITE;
    } else if !i.bits.overwrite_trigger_run() {
        i.bits.set_overwrite_trigger_run(true);
        flags |= UpdateTriggerFlags::OVERWRITE;
        op.new = as_s(&mut deleted);
        op.new_buf_u64s = 0;
    } else if !i.bits.insert_trigger_run() {
        i.bits.set_insert_trigger_run(true);
        flags |= UpdateTriggerFlags::INSERT;
        op.old = BkeySC::from_bkey_i(&deleted);
        trigger = new_ops.trigger;
    }
    op.flags = flags;

    run_trigger(t, trigger, &mut op)?;
    if op.btree.has_snapshots() {
        crate::alloc::buckets::trigger_snapshot_nr_keys(t, &op)?;
    }
    Ok(())
}

/// @trans's updates - looked up by index each time, as C's loops do: a
/// transactional trigger queues more, and a path it allocates can move them
/// all, to a bigger array.
fn nr_updates(trans: &BtreeTrans<'_>) -> usize {
    trans.nr_updates.get().into()
}

/// # Safety
/// @idx is below nr_updates(), and the update not otherwise borrowed.
#[allow(clippy::mut_from_ref)]
unsafe fn update<'a>(trans: &'a BtreeTrans<'_>, idx: usize) -> &'a mut c::btree_insert_entry {
    unsafe { &mut *trans.updates.get().add(idx) }
}

fn trans_triggers_run(i: &c::btree_insert_entry) -> bool {
    i.flags & UpdateTriggerFlags::NORUN.bits() != 0 ||
        !i.has_trans_triggers() ||
        (i.bits.insert_trigger_run() && i.bits.overwrite_trigger_run())
}

/// bch2_trans_commit_run_triggers(): the commit's transactional triggers -
/// its updates', and those of the updates they queue.
///
/// # Safety
/// In @trans's commit.
#[c_export]
pub unsafe fn bch2_trans_commit_run_triggers(trans: &BtreeTrans<'_>) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(unsafe { run_trans_triggers(trans, &t) })
}

unsafe fn run_trans_triggers(trans: &BtreeTrans<'_>, t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
    let mut sort_id_start = 0;

    while sort_id_start < nr_updates(trans) {
        let sort_id = unsafe { update(trans, sort_id_start) }.sort_order;
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
            while idx < nr_updates(trans) && unsafe { update(trans, idx) }.sort_order <= sort_id {
                let i = unsafe { update(trans, idx) };
                if i.sort_order < sort_id {
                    sort_id_start = idx;
                } else if !trans_triggers_run(i) {
                    unsafe { run_one_trans_trigger(trans, t, i) }?;
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
        for idx in 0..nr_updates(trans) {
            assert!(trans_triggers_run(unsafe { update(trans, idx) }),
                    "update {idx} of {}: transactional triggers not run", nr_updates(trans));
        }
    }
    Ok(())
}

/// The commit's atomic triggers, then gc's, if it's running: those for the
/// updates its walk has passed.
///
/// # Safety
/// In @trans's commit, its atomic phase.
#[c_export]
pub unsafe fn bch2_trans_commit_run_mem_triggers(trans: &BtreeTrans<'_>) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(unsafe { run_mem_triggers(trans, &t) })
}

unsafe fn run_mem_triggers(trans: &BtreeTrans<'_>, t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
    for idx in 0..nr_updates(trans) {
        let i = unsafe { update(trans, idx) };
        let flags = UpdateTriggerFlags::from_bits_retain(i.flags);

        if i.has_atomic_triggers() && !flags.contains(UpdateTriggerFlags::NORUN) {
            unsafe { run_one_mem_trigger(trans, t, i, flags | UpdateTriggerFlags::ATOMIC) }?;
        }
    }

    if trans.fs().gc.phase() != c::gc_phase::GC_PHASE_not_running {
        unsafe { run_gc_triggers(trans, t) }?;
    }
    Ok(())
}

#[cold]
unsafe fn run_gc_triggers(trans: &BtreeTrans<'_>, t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
    let gc = &trans.fs().gc;

    for idx in 0..nr_updates(trans) {
        let i = unsafe { update(trans, idx) };
        let flags = UpdateTriggerFlags::from_bits_retain(i.flags);

        if i.has_triggers() && !flags.contains(UpdateTriggerFlags::NORUN) &&
           gc.visited(c::gc_pos::in_btree(i.bits.btree_id(), i.bits.level().into(), unsafe { (*i.k).k.p })) {
            unsafe { run_one_mem_trigger(trans, t, i, flags | UpdateTriggerFlags::GC) }?;
        }
    }
    Ok(())
}
