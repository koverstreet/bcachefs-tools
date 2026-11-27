// SPDX-License-Identifier: GPL-2.0

//! Fast, unordered lists - add, remove and walk, all lockless: a slot each,
//! in a radix tree of item pointers. Slot numbers come from an IDA through a
//! per-CPU cache of free ones, so only refilling or spilling a cache takes
//! the IDA's lock.
//!
//! A slot can be had before its item - get_slot() - to take the allocation
//! failure early, and filled where failing isn't an option: Slot::set().
//! Slot 0 is never handed out: an index of 0 is no slot, which remove()
//! ignores.
//!
//! A cache is changed with interrupts off - an IrqGuard - as items come off
//! in bio completions. It holds 31: a refill gets up to 16, and a full one
//! spills 16 back. A refill allocates with interrupts on, so the cache may
//! have been refilled meanwhile: what doesn't fit goes back to the IDA, and
//! the cache, full, has a slot to give.
//!
//! The walk is items_from(): under RCU, the slots read as they change. add() publishes the item with a
//! release, which items_from()'s acquire pairs with; remove() is a plain
//! store of NULL.

use core::iter;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering::{Acquire, Relaxed, Release}};
use crate::data_structures::{array_vec::ArrayVec, genradix::{self, SharedGenRadix}, ida::Ida};
use crate::errcode::BchError;
use crate::os::percpu::{IrqGuard, PerCpu};
use crate::util::{alloc::AllocError, rcu::ReadGuard};
use crate::warn_on;
use cstruct_macros::MaybeZeroable;
use pin_init::{pin_data, pin_init, pinned_drop, PinInit, Zeroable};

#[repr(C)]
#[pin_data(PinnedDrop)]
#[derive(MaybeZeroable)]
pub struct FastList<T> {
    /* the items, by slot: allocated by any CPU at once */
    pub items: SharedGenRadix<Item<T>>,
    #[pin]
    pub slots_allocated: Ida,
    buffer: PerCpu<Cache>,
}

/// A slot's item: other CPUs change it as it's read.
#[repr(transparent)]
pub struct Item<T>(AtomicPtr<T>);

// SAFETY: zeroed is null, no item
unsafe impl<T> Zeroable for Item<T> {}

// genradix_entry!()'s, which can't size a generic leaf: every Item is a pointer
impl<T> genradix::Entry for Item<T> {
    type Leaf = [Item<T>; genradix::NODE_SIZE / size_of::<AtomicPtr<()>>()];
}

/// What a refill gets, and a full cache spills: half of it.
const BATCH: usize = 16;

/// A CPU's cache of free slots: with its count, 128 bytes.
struct Cache(ArrayVec<u32, 31>);

// SAFETY: zeroed is empty - an ArrayVec is its length and its elements'
// MaybeUninits
unsafe impl Zeroable for Cache {}

impl<T> FastList<T> {
    /// @f on this CPU's cache, interrupts off.
    fn with_cache<R>(&self, f: impl FnOnce(&mut ArrayVec<u32, 31>) -> R) -> R {
        let irq = IrqGuard::new();
        // SAFETY: only this CPU's own uses touch its copy, which interrupts
        // off keeps to this one
        f(&mut unsafe { self.buffer.this_cpu(&irq).as_mut() }.0)
    }

    fn free_slots(&self, idxs: impl IntoIterator<Item = u32>) {
        for idx in idxs {
            self.slots_allocated.free(idx);
        }
    }

    /// @idx's slot: get_idx() allocated its node.
    fn slot(&self, idx: u32) -> &AtomicPtr<T> {
        &self.items.get(idx as usize).expect("a fast_list slot get_idx() didn't allocate").0
    }

    /// The items from slot @idx on, with their slots. Other CPUs change the
    /// slots as it's read: each is read once, atomically.
    ///
    /// XXX: an item is only safe to use if it's freed a grace period after
    /// removal, and nothing here says so. Next: items &'g T, with an unsafe
    /// marker on T the owner asserts that with. async_objs' items don't hold to
    /// it today: see debug/async_objs.rs.
    pub fn items_from<'g>(&'g self, idx: usize, _rcu: &'g ReadGuard)
        -> impl Iterator<Item = (usize, NonNull<T>)> + 'g
    {
        self.items.iter_from(idx).filter_map(|(i, slot)| NonNull::new(slot.0.load(Acquire)).map(|item| (i, item)))
    }

    /// A new slot from the IDA, its radix node allocated.
    fn alloc_idx(&self) -> Option<u32> {
        let idx = self.slots_allocated.alloc_range(1, i32::MAX as u32).ok()?;
        if self.items.prealloc(idx as usize).is_err() {
            self.free_slots([idx]);
            return None;
        }
        Some(idx)
    }

    /// A slot for an item to be set later, its allocation failing now if it's
    /// to fail - as the caller's memalloc scope allows.
    pub fn get_slot(&self) -> Result<Slot<'_, T>, AllocError> {
        Ok(Slot { list: self, idx: self.get_idx()? })
    }

    fn get_idx(&self) -> Result<u32, AllocError> {
        if let Some(idx) = self.with_cache(|lp| lp.pop()) {
            return Ok(idx);
        }

        let mut got: ArrayVec<u32, BATCH> = iter::from_fn(|| self.alloc_idx()).take(BATCH).collect();
        let idx = self.with_cache(|lp| {
            let fit = got.len().min(lp.remaining_capacity());
            lp.extend(got.drain(got.len() - fit..));
            lp.pop()
        });
        // what didn't fit - the cache was refilled meanwhile - goes back
        self.free_slots(got);
        idx.ok_or(AllocError)
    }

    /// Gives back a slot get_idx() handed out, never set.
    fn put_idx(&self, idx: u32) {
        if idx == 0 {
            return;
        }

        let spill = self.with_cache(|lp| {
            let spill: ArrayVec<u32, BATCH> =
                if lp.is_full() { lp.drain(lp.len() - BATCH..).collect() } else { ArrayVec::new() };
            lp.push(idx);
            spill
        });
        self.free_slots(spill);
    }

    /// @item in a new slot: the slot, for remove().
    pub fn add(&self, item: NonNull<T>) -> Result<u32, AllocError> {
        Ok(self.get_slot()?.set(item))
    }

    /// @item in slot @idx, a Slot's into_idx().
    pub fn set(&self, idx: u32, item: NonNull<T>) {
        self.slot(idx).store(item.as_ptr(), Release);
    }

    /// Empties slot @idx, and gives it back.
    pub fn remove(&self, idx: u32) {
        if idx == 0 {
            return;
        }
        self.slot(idx).store(null_mut(), Relaxed);
        self.put_idx(idx);
    }

    pub fn new() -> impl PinInit<Self, BchError> {
        pin_init!(Self {
            items: SharedGenRadix::new(),
            slots_allocated <- Ida::new(),
            buffer: PerCpu::alloc()?,
        }? BchError)
    }

    /// Made: a static's is zeroed until it's made in place.
    pub fn is_made(&self) -> bool {
        self.buffer.allocated()
    }
}

/// Every item should be off it.
#[pinned_drop]
impl<T> PinnedDrop for FastList<T> {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: no CPU's using it
        for lp in self.buffer.get().cpus() {
            self.free_slots(unsafe { lp.as_mut() }.0.drain(..));
        }

        // a slot still allocated is an item still on the list
        warn_on!(self.slots_allocated.find_first().is_some());

        unsafe { self.items.free() };
    }
}

/// A slot had before its item, empty: given back if it's dropped so.
#[must_use = "a slot dropped is given back"]
pub struct Slot<'l, T> {
    list: &'l FastList<T>,
    idx:  u32,
}

impl<T> Slot<'_, T> {
    /// @item in it: its index, for remove().
    pub fn set(self, item: NonNull<T>) -> u32 {
        let list = self.list;
        let idx = self.into_idx();
        list.set(idx, item);
        idx
    }

    /// Its index, the slot kept: for an owner that set()s or remove()s it
    /// by index, later.
    pub fn into_idx(self) -> u32 {
        ManuallyDrop::new(self).idx
    }
}

impl<T> Drop for Slot<'_, T> {
    fn drop(&mut self) {
        self.list.put_idx(self.idx)
    }
}
