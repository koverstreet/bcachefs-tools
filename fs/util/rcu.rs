// SPDX-License-Identifier: GPL-2.0

//! RCU: read-side sections, and buffers published under them.
//!
//! read_lock() is rcu_read_lock() until the guard drops: the kernel's, or in
//! userspace liburcu's - the memb flavor, which the tools link and C's
//! rcu_read_lock() is.
//!
//! RcuBuf<T> is C's `T __rcu *`, laid out as C's pointer, so C still reads
//! it. A reader gets a &T for as long as its ReadGuard. A writer - serialized
//! by a lock the owner keeps, which RcuBuf can't see, hence unsafe - replaces
//! it, and the old buffer is freed with kvfree_rcu() once the readers that
//! might hold it are gone. RcuArray<T> is an RcuBuf of C's
//! `{ rcu_head rcu; size_t nr; T s[]; }`, read as a &[T], and grown by copy
//! and replace.
//!
//! Buffers are kvmalloc()'d and freed through C - a kvfree_rcu()'d buffer
//! has to come from C's allocator - and freed as bytes: what they hold has no
//! Drop.

use core::marker::PhantomData;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::c;
use crate::util::alloc::AllocError;

#[cfg(not(kernel))]
unsafe extern "C" {
    fn urcu_memb_read_lock();
    fn urcu_memb_read_unlock();
}

/// An RCU read-side critical section, open until dropped. Not Send: it's
/// the thread's.
pub struct ReadGuard {
    #[cfg(kernel)]
    _guard:   kernel::sync::rcu::Guard,
    _not_send: PhantomData<*mut ()>,
}

/// rcu_read_lock(), until the guard drops.
pub fn read_lock() -> ReadGuard {
    #[cfg(not(kernel))]
    unsafe { urcu_memb_read_lock() };

    ReadGuard {
        #[cfg(kernel)]
        _guard:   kernel::sync::rcu::read_lock(),
        _not_send: PhantomData,
    }
}

#[cfg(not(kernel))]
impl Drop for ReadGuard {
    fn drop(&mut self) {
        unsafe { urcu_memb_read_unlock() };
    }
}

/// rcu_dereference(*@p): the pointer, as published - good while @_g is.
///
/// # Safety
/// @p is a pointer RCU publishes, and the caller only uses what it points
/// to within @_g's section.
pub unsafe fn dereference<T>(p: *const *mut T, _g: &ReadGuard) -> *mut T {
    unsafe { (*(p as *const AtomicPtr<T>)).load(Ordering::Acquire) }
}

// ── RcuBuf ───────────────────────────────────────────────────────────────

/// What an RcuBuf can point to: a buffer starting with an rcu_head, which a
/// thin pointer - C's - reaches all of: sized, or a DST whose head records its
/// length.
///
/// # Safety
/// Head starts with a c::rcu_head; from_thin() of a pointer to the Head of a
/// valid Self is a pointer to all of it. Self has no Drop.
pub unsafe trait RcuPointee {
    type Head;

    /// # Safety
    /// @p points to the Head of a valid Self.
    unsafe fn from_thin(p: *mut Self::Head) -> *mut Self;
}

/// C's `T __rcu *`: see the top of the file.
#[repr(transparent)]
pub struct RcuBuf<T: ?Sized + RcuPointee> {
    p:  AtomicPtr<T::Head>,
    _t: PhantomData<*const T>,
}

// SAFETY: readers on any thread share the T, and the writer's thread frees it
unsafe impl<T: ?Sized + RcuPointee + Send + Sync> Send for RcuBuf<T> {}
unsafe impl<T: ?Sized + RcuPointee + Send + Sync> Sync for RcuBuf<T> {}

// SAFETY: zeroed is NULL, nothing published
unsafe impl<T: ?Sized + RcuPointee> ::pin_init::Zeroable for RcuBuf<T> {}

impl<T: ?Sized + RcuPointee> RcuBuf<T> {
    /// rcu_dereference(): what's published, for as long as @_g's section.
    pub fn read<'a>(&'a self, _g: &'a ReadGuard) -> Option<&'a T> {
        let p = self.p.load(Ordering::Acquire);
        // SAFETY: what's published is a valid T, and a replaced one is freed
        // only after a grace period - after @_g's section ends
        (!p.is_null()).then(|| unsafe { &*T::from_thin(p) })
    }

    /// rcu_dereference_protected(): the writer's view.
    ///
    /// # Safety
    /// The caller holds the writers' lock, until it's done with the &T.
    pub unsafe fn get_locked(&self) -> Option<&T> {
        let p = self.p.load(Ordering::Relaxed);
        // SAFETY: only a writer replaces or frees it, and we're the writer
        (!p.is_null()).then(|| unsafe { &*T::from_thin(p) })
    }

    /// rcu_assign_pointer() @new, and kvfree_rcu() what it replaces.
    ///
    /// # Safety
    /// The caller holds the writers' lock. @new is a valid T from
    /// rust_kvzalloc(), which this takes.
    pub unsafe fn replace(&self, new: NonNull<T::Head>) {
        let old = self.p.swap(new.as_ptr(), Ordering::Release);
        if !old.is_null() {
            // SAFETY: its Head starts with an rcu_head; readers still in it
            // are what the grace period waits out
            unsafe { c::rust_kvfree_rcu(old.cast()) };
        }
    }
}

impl<T: ?Sized + RcuPointee> Drop for RcuBuf<T> {
    /// No readers are left: whatever held this is going away.
    fn drop(&mut self) {
        let p = *self.p.get_mut();
        if !p.is_null() {
            // SAFETY: from rust_kvzalloc(), and nothing reads it now
            unsafe { c::rust_kvfree(p.cast()) };
        }
    }
}

// ── RcuArray ─────────────────────────────────────────────────────────────

/// RcuArray's buffer: C's `struct { struct rcu_head rcu; size_t nr; T s[]; }`.
#[repr(C)]
pub struct RcuArrayBuf<T> {
    rcu: c::rcu_head,
    nr:  usize,
    s:   [T],
}

/// RcuArrayBuf's head: what C's pointer is to.
#[repr(C)]
pub struct RcuArrayHead {
    rcu: c::rcu_head,
    nr:  usize,
}

// SAFETY: as its elements: rcu is touched only by kvfree_rcu(), freeing it
unsafe impl<T: Send> Send for RcuArrayBuf<T> {}
unsafe impl<T: Sync> Sync for RcuArrayBuf<T> {}

impl<T> RcuArrayBuf<T> {
    /// Where s[] starts - offsetof(), for checking a C struct's layout matches.
    pub const TAIL: usize = size_of::<RcuArrayHead>().next_multiple_of(align_of::<T>());
}

// SAFETY: starts with rcu, and its length is nr; RcuElements have no Drop
unsafe impl<T: RcuElement> RcuPointee for RcuArrayBuf<T> {
    type Head = RcuArrayHead;

    unsafe fn from_thin(p: *mut RcuArrayHead) -> *mut Self {
        // a slice pointer's length is the DST's tail's
        core::ptr::slice_from_raw_parts_mut(p.cast::<T>(), unsafe { (*p).nr }) as *mut Self
    }
}

/// What an RcuArray holds: elements a writer copies bytewise into a grown
/// buffer while readers still read the old one - so a writer changing one in
/// place, with readers looking, needs it to be atomics.
///
/// # Safety
/// A bytewise copy of a T is a valid T, owning nothing: no Drop. All-zero
/// bytes are a valid T.
pub unsafe trait RcuElement: Sync {}

macro_rules! rcu_element {
    ($($t:ty),*) => { $(unsafe impl RcuElement for $t {})* };
}
// SAFETY: plain integers, and atomics of them
rcu_element!(u8, u16, u32, u64, usize,
             core::sync::atomic::AtomicU8, core::sync::atomic::AtomicU16,
             core::sync::atomic::AtomicU32, core::sync::atomic::AtomicU64,
             core::sync::atomic::AtomicUsize);

/// An RcuBuf of an array: see the top of the file.
#[repr(transparent)]
pub struct RcuArray<T: RcuElement>(RcuBuf<RcuArrayBuf<T>>);

// SAFETY: an RcuBuf, zeroed
unsafe impl<T: RcuElement> ::pin_init::Zeroable for RcuArray<T> {}

impl<T: RcuElement> RcuArray<T> {
    /// The elements published, for as long as @g's section: none if nothing
    /// is.
    pub fn read<'a>(&'a self, g: &'a ReadGuard) -> &'a [T] {
        self.0.read(g).map_or(&[], |b| &b.s)
    }

    /// The writer's view.
    ///
    /// # Safety
    /// As RcuBuf::get_locked().
    pub unsafe fn get_locked(&self) -> &[T] {
        unsafe { self.0.get_locked() }.map_or(&[], |b| &b.s)
    }

    /// Room for at least @nr elements: if there isn't, a new buffer, sized to
    /// a power of two bytes as C's tables are, the old elements copied and the
    /// rest zero, published. An error if it would be more than INT_MAX bytes,
    /// which kvmalloc() won't do.
    ///
    /// # Safety
    /// As RcuBuf::replace(): the caller holds the writers' lock.
    pub unsafe fn grow_to(&self, nr: usize) -> Result<(), AllocError> {
        const { assert!(size_of::<T>() != 0) };

        let old = unsafe { self.get_locked() };
        if nr <= old.len() {
            return Ok(());
        }

        let tail = RcuArrayBuf::<T>::TAIL;
        let bytes = nr.checked_mul(size_of::<T>())
            .and_then(|b| b.checked_add(tail))
            .and_then(usize::checked_next_power_of_two)
            .filter(|&b| b <= i32::MAX as usize)
            .ok_or(AllocError)?;

        let new = NonNull::new(unsafe { c::rust_kvzalloc(bytes) }.cast::<RcuArrayHead>())
            .ok_or(AllocError)?;

        // SAFETY: @bytes are ours, zeroed - a valid head and elements but for
        // nr; the old elements don't change under the writers' lock
        unsafe {
            (*new.as_ptr()).nr = (bytes - tail) / size_of::<T>();
            core::ptr::copy_nonoverlapping(old.as_ptr(),
                                           new.as_ptr().byte_add(tail).cast::<T>(),
                                           old.len());
            self.0.replace(new);
        }
        Ok(())
    }
}
