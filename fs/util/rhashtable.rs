// SPDX-License-Identifier: GPL-2.0

//! An rhashtable's read side, under RCU, a bucket at a time. struct
//! bucket_table is opaque - the kernel's bindings only declare it - so its
//! size and slots come from C (util.c); chains are walked here: a slot's bit
//! 0 is its lock, and a chain ends at an odd "nulls" pointer. During a
//! resize it's the old table, as rht_for_each_entry_rcu() reads it.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::c;
use crate::util::rcu::ReadGuard;

pub struct RhtRead<'g, T> {
    tbl:         *const c::bucket_table,
    head_offset: usize,
    _entries:    PhantomData<(&'g T, &'g ReadGuard)>,
}

/// # Safety
/// @ht's entries are Ts, linked by their rhash_head at @head_offset, whose
/// memory stays valid for an RCU section that finds them: freed after a
/// grace period, or never while the table's in use.
pub unsafe fn read<'g, T>(ht: &'g c::rhashtable, head_offset: usize, _rcu: &'g ReadGuard)
    -> RhtRead<'g, T>
{
    RhtRead {
        tbl: unsafe { c::bch2_rht_table(ht) },
        head_offset,
        _entries: PhantomData,
    }
}

impl<'g, T> RhtRead<'g, T> {
    pub fn nr_buckets(&self) -> usize {
        unsafe { c::bch2_rht_nr_buckets(self.tbl) as usize }
    }

    /// rht_for_each_entry_rcu().
    pub fn bucket(&self, i: usize) -> impl Iterator<Item = &'g T> + use<'g, T> {
        assert!(i < self.nr_buckets());

        // the slot and links change as we read: rcu_dereference()d
        let slot = unsafe { c::bch2_rht_bucket(self.tbl, i as u32) };
        let first = unsafe { AtomicPtr::from_ptr(slot) }.load(Ordering::Acquire);
        let mut pos = (first as usize & !1) as *mut c::rhash_head;
        let head_offset = self.head_offset;

        core::iter::from_fn(move || {
            // empty, or the end: a nulls marker
            if pos.is_null() || pos as usize & 1 != 0 {
                return None;
            }
            // SAFETY: read()'s
            unsafe {
                let entry = &*pos.byte_sub(head_offset).cast::<T>();
                pos = AtomicPtr::from_ptr(&raw mut (*pos).next).load(Ordering::Acquire);
                Some(entry)
            }
        })
    }
}
