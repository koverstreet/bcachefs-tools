// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::{key_ops, BkeyS, BkeySC};
use crate::btree::bkey::prelude::*;
use crate::btree::iter::{BtreeTrans, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::format::{Bpos, Btree};
use core::ffi::{c_int, c_uint};
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
            new:   BkeyS { k: unsafe { &mut *new.k }, v: new.v, val: core::marker::PhantomData },
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
        self.new = BkeyS { k: unsafe { &mut *out.k }, v: out.v, val: core::marker::PhantomData };
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

/// # Safety
/// @op is one the commit is running triggers for: both keys valid, @new the
/// trigger's to edit.
unsafe fn run_trigger(t: &TransAttempt<'_, '_>, trigger: Option<&'static Trigger>, op: &mut c::btree_trigger_op)
    -> Result<(), BchError>
{
    match trigger {
        Some(trigger) => (trigger.0)(t, &mut unsafe { TriggerOp::from_raw(op) }),
        None          => Ok(()),
    }
}

/// bch2_key_trigger(): @op's key's trigger - the old key's type's, the new
/// key's if it's a deletion - then its snapshot's count of keys.
///
/// # Safety
/// As run_trigger().
unsafe fn key_trigger(t: &TransAttempt<'_, '_>, op: &mut c::btree_trigger_op) -> Result<(), BchError> {
    let ty = match unsafe { (*op.old.k).type_ } {
        0  => unsafe { (*op.new.split.kv.k).type_ },
        ty => ty,
    };
    unsafe { run_trigger(t, key_ops(ty).trigger, op) }?;

    /*
     * Gate matches the trans-commit dispatch: only snapshotted btrees get
     * per-snapshot key counts. Snapshot-field btrees (reconcile_pending)
     * carry a snapshot in their bpos but must not be counted.
     */
    if op.btree.has_snapshots() {
        crate::alloc::buckets::trigger_snapshot_nr_keys(t, &unsafe { TriggerOp::from_raw(op) })?;
    }
    Ok(())
}

/// # Safety
/// @op is one C runs a trigger for, in its commit's attempt.
#[c_export]
pub unsafe fn bch2_key_trigger(trans: &BtreeTrans<'_>, mut op: c::btree_trigger_op) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(unsafe { key_trigger(&t, &mut op) })
}

/// A deletion at @p, for the side of an update that has no key.
fn deleted_at(p: Bpos) -> c::bkey_i {
    c::bkey_i { k: c::bkey { p, ..c::bkey::new() }, ..Default::default() }
}

fn raw_s(k: &mut c::bkey_i) -> c::bkey_s {
    c::bkey_s { split: c::bkey_s_split { kv: c::bkey_s_kv { k: &mut k.k, v: &mut k.v } } }
}

fn raw_s_c(k: &c::bkey_i) -> c::bkey_s_c {
    c::bkey_s_c { k: &k.k, v: &k.v }
}

fn raw_flags(f: UpdateTriggerFlags) -> c::btree_iter_update_trigger_flags {
    c::btree_iter_update_trigger_flags::from_bits_retain(f.bits())
}

/// run_one_mem_trigger(): @i's triggers in the atomic phase, or gc's, with
/// @flags - its snapshot's count of keys first, from its old key to its new,
/// then its key type's trigger, once, with both keys, where the old and new
/// types share one.
///
/// # Safety
/// @i is an update of @trans's, in its commit.
#[c_export]
pub unsafe fn bch2_run_one_mem_trigger(trans: &BtreeTrans<'_>, i: &mut c::btree_insert_entry, flags: c_uint)
    -> c_int
{
    let t = unsafe { trans.attempt_in_progress() };
    let flags = UpdateTriggerFlags::from_bits_retain(flags);
    let new = unsafe { &mut *i.k };
    let mut deleted = deleted_at(new.k.p);
    let old_k = i.old_k;
    let (old_ops, new_ops) = (key_ops(old_k.type_), key_ops(new.k.type_));

    let mut op = c::btree_trigger_op {
        btree:        i.bits.btree_id(),
        level:        i.bits.level().into(),
        old:          c::bkey_s_c { k: &old_k, v: i.old_v },
        new:          raw_s(new),
        new_buf_u64s: i.k_buf_u64s.into(),
        flags:        raw_flags(flags),
    };

    let ret = (|| {
        // a net count, old to new: before the split below rewrites @op
        if op.btree.has_snapshots() {
            crate::alloc::buckets::trigger_snapshot_nr_keys(&t, &unsafe { TriggerOp::from_raw(&mut op) })?;
        }

        if old_ops.trigger.is_some() && same_trigger(old_ops.trigger, new_ops.trigger) {
            op.flags = raw_flags(flags | UpdateTriggerFlags::INSERT | UpdateTriggerFlags::OVERWRITE);
            return unsafe { run_trigger(&t, old_ops.trigger, &mut op) };
        }

        if new_ops.trigger.is_some() {
            op.flags = raw_flags(flags | UpdateTriggerFlags::INSERT);
            op.old   = raw_s_c(&deleted);
            unsafe { run_trigger(&t, new_ops.trigger, &mut op) }?;
        }

        if old_ops.trigger.is_some() {
            op.flags        = raw_flags(flags | UpdateTriggerFlags::OVERWRITE);
            op.old          = c::bkey_s_c { k: &old_k, v: i.old_v };
            op.new          = raw_s(&mut deleted);
            op.new_buf_u64s = 0;
            unsafe { run_trigger(&t, old_ops.trigger, &mut op) }?;
        }
        Ok(())
    })();
    ret_to_c(ret)
}

/// run_one_trans_trigger(): the next of @i's transactional triggers not run -
/// both at once where the old and new types share one, else the overwrite,
/// then the insert - then its snapshot's count of keys. A trigger can queue
/// more updates, so it's handed copies, not @i.
///
/// # Safety
/// As bch2_run_one_mem_trigger().
#[c_export]
pub unsafe fn bch2_run_one_trans_trigger(trans: &BtreeTrans<'_>, i: &mut c::btree_insert_entry) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    let new = unsafe { &mut *i.k };
    let mut deleted = deleted_at(new.k.p);
    let old_k = i.old_k;
    let (old_ops, new_ops) = (key_ops(old_k.type_), key_ops(new.k.type_));
    let mut flags = UpdateTriggerFlags::from_bits_retain(i.flags) | UpdateTriggerFlags::TRANSACTIONAL;

    let mut op = c::btree_trigger_op {
        btree:        i.bits.btree_id(),
        level:        i.bits.level().into(),
        old:          c::bkey_s_c { k: &old_k, v: i.old_v },
        new:          raw_s(new),
        new_buf_u64s: i.k_buf_u64s.into(),
        flags:        raw_flags(flags),
    };

    let mut trigger = old_ops.trigger;
    if !i.bits.insert_trigger_run() && !i.bits.overwrite_trigger_run() &&
       same_trigger(old_ops.trigger, new_ops.trigger) {
        i.bits.set_overwrite_trigger_run(true);
        i.bits.set_insert_trigger_run(true);
        flags |= UpdateTriggerFlags::INSERT | UpdateTriggerFlags::OVERWRITE;
    } else if !i.bits.overwrite_trigger_run() {
        i.bits.set_overwrite_trigger_run(true);
        flags |= UpdateTriggerFlags::OVERWRITE;
        op.new = raw_s(&mut deleted);
        op.new_buf_u64s = 0;
    } else if !i.bits.insert_trigger_run() {
        i.bits.set_insert_trigger_run(true);
        flags |= UpdateTriggerFlags::INSERT;
        op.old = raw_s_c(&deleted);
        trigger = new_ops.trigger;
    }
    op.flags = raw_flags(flags);

    let ret = unsafe { run_trigger(&t, trigger, &mut op) }.and_then(|()| {
        if op.btree.has_snapshots() {
            crate::alloc::buckets::trigger_snapshot_nr_keys(&t, &unsafe { TriggerOp::from_raw(&mut op) })
        } else {
            Ok(())
        }
    });
    ret_to_c(ret)
}
