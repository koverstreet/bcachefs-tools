// SPDX-License-Identifier: GPL-2.0

//! Fast, unordered lists - add, remove and walk, all lockless: a slot each,
//! in a radix tree of item pointers. Slot numbers come from an IDA through a
//! per-CPU cache of free ones, so only refilling or spilling a cache takes
//! the IDA's lock.
//!
//! A slot can be had before its item - get_idx() - to take the allocation
//! failure early, and filled where failing isn't an option: C's
//! fast_list_set(). Slot 0 is never handed out: an index of 0 is no slot,
//! which put_idx() and remove() ignore.
//!
//! A cache is changed with interrupts off - an IrqGuard - as items come off
//! in bio completions. It holds 31: a refill gets up to 16, and a full one
//! spills 16 back. A refill allocates with interrupts on, so the cache may
//! have been refilled meanwhile: what doesn't fit goes back to the IDA, and
//! the cache, full, has a slot to give.
//!
//! The walk is the C header's fast_list_for_each() and items_from(): under
//! RCU, the slots read as they change. add() publishes the item with a
//! release, which items_from()'s acquire pairs with; remove() is a plain
//! store of NULL.
//!
//! C's users are the VFS inode list (vfs/fs.c) and async_objs.h's macros.

use crate::c;
use crate::util::alloc::AllocError;
use crate::util::percpu::{IrqGuard, PerCpu, PerCpuRef};
use crate::util::rcu::ReadGuard;
use arrayvec::ArrayVec;
use core::ffi::{c_int, c_uint, c_void};
use core::iter;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use cstruct_macros::c_export;

/// What a refill gets, and a full cache spills: half of it.
const BATCH: usize = 16;

/// A CPU's cache of free slots - C's opaque fast_list_pcpu: with its count,
/// 128 bytes.
struct Cache(ArrayVec<u32, 31>);

// SAFETY: zeroed is empty - an ArrayVec is its length and its elements'
// MaybeUninits
unsafe impl ::pin_init::Zeroable for Cache {}

impl c::fast_list {
    fn ida(&self) -> *mut c::ida {
        self.slots_allocated.as_ptr()
    }

    /// @f on this CPU's cache, interrupts off.
    fn with_cache<R>(&self, f: impl FnOnce(&mut ArrayVec<u32, 31>) -> R) -> R {
        let irq = IrqGuard::new();
        // SAFETY: init()'s Caches, freed by exit() after the list's last use
        let caches = unsafe { PerCpuRef::<Cache>::from_raw(self.buffer.load(Relaxed).cast()) };
        // SAFETY: only this CPU's own uses touch its copy, which interrupts
        // off keeps to this one
        f(&mut unsafe { caches.this_cpu(&irq).as_mut() }.0)
    }

    fn free_slots(&self, idxs: impl IntoIterator<Item = u32>) {
        for idx in idxs {
            // SAFETY: init()'s ida, and slots it handed out
            unsafe { c::ida_free(self.ida(), idx) };
        }
    }

    /// @idx's slot: get_idx() allocated its node.
    fn slot(&self, idx: u32) -> &AtomicPtr<c_void> {
        let p = self.items.get_raw(idx as usize).expect("a fast_list slot get_idx() didn't allocate");
        // SAFETY: a slot of the radix, a pointer, which nodes never move
        unsafe { AtomicPtr::from_ptr(p.as_ptr()) }
    }

    /// fast_list_for_each_from(), with each item's slot. Other CPUs change the
    /// slots as it's read: each is read once, atomically.
    ///
    /// XXX: an item is only safe to use if it's freed a grace period after
    /// removal, and nothing here says so. Once the owners are Rust: fast_list<T>,
    /// items &'g T, with an unsafe marker on T the owner asserts that with.
    /// async_objs' items don't hold to it today: see debug/async_objs.rs.
    pub fn items_from<'g>(&'g self, idx: usize, _rcu: &'g ReadGuard)
        -> impl Iterator<Item = (usize, NonNull<c_void>)> + 'g
    {
        self.items.iter_from_raw(idx).filter_map(|(i, slot)| {
            // SAFETY: as slot()'s
            let item = unsafe { AtomicPtr::from_ptr(slot.as_ptr()) }.load(Acquire);
            NonNull::new(item).map(|item| (i, item))
        })
    }

    /// A new slot from the IDA, its radix node allocated.
    fn alloc_idx(&self, gfp: c::gfp_t) -> Option<u32> {
        // SAFETY: init()'s ida
        let idx = unsafe { c::ida_alloc_range(self.ida(), 1, i32::MAX as c_uint, gfp) };
        let idx = u32::try_from(idx).ok()?;
        if self.items.prealloc(idx as usize, gfp).is_err() {
            self.free_slots([idx]);
            return None;
        }
        Some(idx)
    }

    /// A slot for an item to be set later, its allocation failing now if it's
    /// to fail.
    pub fn get_idx(&self, gfp: c::gfp_t) -> Result<u32, AllocError> {
        if let Some(idx) = self.with_cache(|lp| lp.pop()) {
            return Ok(idx);
        }

        let mut got: ArrayVec<u32, BATCH> = iter::from_fn(|| self.alloc_idx(gfp)).take(BATCH).collect();
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
    pub fn put_idx(&self, idx: u32) {
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
    pub fn add(&self, item: NonNull<c_void>) -> Result<u32, AllocError> {
        let idx = self.get_idx(c::GFP_KERNEL)?;
        self.slot(idx).store(item.as_ptr(), Release);
        Ok(idx)
    }

    /// Empties slot @idx, and gives it back.
    pub fn remove(&self, idx: u32) {
        if idx == 0 {
            return;
        }
        self.slot(idx).store(null_mut(), Relaxed);
        self.put_idx(idx);
    }

    /// # Safety
    /// Nothing else is using it: it's zeroed, or exit()ed.
    pub unsafe fn init(&self) -> Result<(), AllocError> {
        unsafe { c::ida_init(self.ida()) };
        self.buffer.store(PerCpu::<Cache>::alloc()?.into_raw().cast(), Relaxed);
        Ok(())
    }

    /// Frees what init() and the list allocated; every item should be off it.
    ///
    /// # Safety
    /// Nothing is using it, or will after - but init() again.
    pub unsafe fn exit(&self) {
        let buffer = self.buffer.swap(null_mut(), Relaxed);
        if !buffer.is_null() {
            // SAFETY: init()'s, and no CPU's using it
            for lp in unsafe { PerCpuRef::<Cache>::from_raw(buffer.cast()) }.cpus() {
                self.free_slots(unsafe { lp.as_mut() }.0.drain(..));
            }
            unsafe { c::free_percpu(buffer.cast()) };
        }

        // a slot still allocated is an item still on the list
        crate::warn_on!(unsafe { c::ida_find_first(self.ida()) } >= 0);

        unsafe {
            c::ida_destroy(self.ida());
            self.items.free();
        }
    }
}

// ── For C ────────────────────────────────────────────────────────────────

/// A slot, or -ENOMEM.
#[c_export]
pub fn fast_list_get_idx(l: &c::fast_list, gfp: c::gfp_t) -> c_int {
    l.get_idx(gfp).map_or(-(c::ENOMEM as c_int), |idx| idx as c_int)
}

#[c_export]
pub fn fast_list_put_idx(l: &c::fast_list, idx: c_uint) {
    l.put_idx(idx)
}

/// @item's slot, or -ENOMEM.
#[c_export]
pub fn fast_list_add(l: &c::fast_list, item: *mut c_void) -> c_int {
    let item = NonNull::new(item).expect("a NULL item for a fast_list");
    l.add(item).map_or(-(c::ENOMEM as c_int), |idx| idx as c_int)
}

#[c_export]
pub fn fast_list_remove(l: &c::fast_list, idx: c_uint) {
    l.remove(idx)
}

/// # Safety
/// As init()'s.
#[c_export]
pub unsafe fn fast_list_init(l: &c::fast_list) -> c_int {
    unsafe { l.init() }.map_or(-(c::ENOMEM as c_int), |()| 0)
}

/// # Safety
/// As exit()'s.
#[c_export]
pub unsafe fn fast_list_exit(l: &c::fast_list) {
    unsafe { l.exit() }
}
