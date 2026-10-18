// SPDX-License-Identifier: GPL-2.0

//! Rust counterparts to the primitives in util/locking.h.
//!
//! bcachefs establishes a memory-reclaim scope (PF_MEMALLOC_NOIO) while holding
//! certain locks, so allocations under the lock can't recurse into reclaim IO -
//! a filesystem driving the block layer directly mustn't let reclaim loop back
//! through the device it's allocating for. On the C side the lock type carries
//! the scope (`struct mutex_noio`); on the Rust side the guard is composed
//! explicitly, holding a [`MemallocFlags`] alongside the lock.
//!
//! [`MemallocFlags`] saves the current flags on construction and restores them
//! on drop, mirroring the C memalloc_flags_save/restore pairing - scoped (LIFO)
//! use only, as those primitives require. The flag itself stays on the C side
//! (util/locking.h's rust_memalloc_* shims): the PF_MEMALLOC_* #defines don't
//! reach Rust through bindgen.

use crate::c;
use core::cell::UnsafeCell;
use core::ffi::{c_void, CStr};
use core::marker::{PhantomData, PhantomPinned};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

/// RAII guard for a process-context memalloc scope. Restores the previous
/// flags when dropped.
#[must_use = "the memalloc scope ends as soon as the guard is dropped"]
pub struct MemallocFlags {
    saved: u32,
}

impl MemallocFlags {
    /// Enter a PF_MEMALLOC_NOIO scope: allocations may not recurse into reclaim
    /// IO for as long as the guard is held.
    pub fn noio() -> Self {
        // SAFETY: rust_memalloc_noio_save only touches current->flags and
        // returns the previous value, to be replayed by the paired restore.
        Self { saved: unsafe { c::rust_memalloc_noio_save() } }
    }
}

impl Drop for MemallocFlags {
    fn drop(&mut self) {
        // SAFETY: `saved` is the value returned by the paired save above.
        unsafe { c::rust_memalloc_flags_restore(self.saved) }
    }
}

/// A C struct's mutex, held: mutex_unlock() on drop - guard(mutex).
#[must_use = "the mutex is unlocked as soon as the guard is dropped"]
pub struct MutexGuard<'a>(&'a c::mutex);

/// mutex_lock(): the C code that shares @m locks it the same way.
pub fn mutex_lock(m: &c::mutex) -> MutexGuard<'_> {
    // SAFETY: a mutex is changed through a shared pointer - it's what it's for
    unsafe { c::mutex_lock(core::ptr::from_ref(m).cast_mut()) };
    MutexGuard(m)
}

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        unsafe { c::mutex_unlock(core::ptr::from_ref(self.0).cast_mut()) };
    }
}

/// A T that a C struct mutex_noio guards: C takes the same lock -
/// guard(mutex_noio) - and Rust reaches the T only through a guard, which is
/// the NOIO scope as C's is. Laid out as C's two members, the lock then the T,
/// so a struct C shares can declare it as them: #[c_anon("struct mutex_noio
/// lock; T v")].
#[repr(C)]
pub struct MutexNoio<T> {
    lock: LockCell,
    data: UnsafeCell<T>,
}

/// The lock: pinned - a kernel mutex mustn't move - and transparent, so C
/// sees only the struct mutex_noio.
#[repr(transparent)]
struct LockCell(UnsafeCell<c::mutex_noio>, PhantomPinned);

impl LockCell {
    fn get(&self) -> *mut c::mutex {
        // SAFETY: a field of it, not a read
        unsafe { &raw mut (*self.0.get()).lock }
    }
}

// SAFETY: a lock: the T is reached only with it held
unsafe impl<T: Send> Send for MutexNoio<T> {}
unsafe impl<T: Send> Sync for MutexNoio<T> {}

impl<T> MutexNoio<T> {
    /// mutex_init(), with a lockdep class of its own - @key, named @name.
    /// new_mutex_noio!() is this, with a static key per call site.
    #[doc(hidden)]
    pub fn new(t: T, name: &'static CStr, key: *mut c_void) -> impl ::pin_init::PinInit<Self> {
        // SAFETY: the mutex is initialized in place - it's pinned, and first in
        // the transparent cell's mutex_noio - and the value written; that's all
        // of it
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                c::rust_mutex_init((&raw mut (*slot).lock).cast(), name.as_ptr().cast(), key);
                (&raw mut (*slot).data).write(UnsafeCell::new(t));
                Ok(())
            })
        }
    }

    pub fn lock(&self) -> MutexNoioGuard<'_, T> {
        let noio = MemallocFlags::noio();
        unsafe { c::mutex_lock(self.lock.get()) };
        MutexNoioGuard { m: self, noio: Some(noio), _not_send: PhantomData }
    }

    /// The lock, as a caller holds it - C's, calling in: its guard isn't ours
    /// to release, and nor is the NOIO scope it's in.
    ///
    /// # Safety
    /// The caller holds the lock, and holds it for as long as this guard
    /// lives; nothing else this task holds has the T.
    pub unsafe fn held(&self) -> MutexNoioGuard<'_, T> {
        unsafe { c::rust_mutex_assert_held(self.lock.get()) };
        MutexNoioGuard { m: self, noio: None, _not_send: PhantomData }
    }
}

/// A held MutexNoio: unlocked when dropped, the NOIO scope ended after - if
/// lock() took it; held()'s is the caller's.
#[must_use = "the mutex is unlocked as soon as the guard is dropped"]
pub struct MutexNoioGuard<'a, T> {
    m:         &'a MutexNoio<T>,
    noio:      Option<MemallocFlags>,
    _not_send: PhantomData<*mut ()>,
}

impl<T> Deref for MutexNoioGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: held
        unsafe { &*self.m.data.get() }
    }
}

impl<T> DerefMut for MutexNoioGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: held
        unsafe { &mut *self.m.data.get() }
    }
}

impl<T> Drop for MutexNoioGuard<'_, T> {
    fn drop(&mut self) {
        if self.noio.is_some() {
            unsafe { c::mutex_unlock(self.m.lock.get()) };
        }
    }
}

/// A MutexNoio initializer for @t, named @name for lockdep, with a lock class
/// of its own.
#[macro_export]
macro_rules! new_mutex_noio {
    ($t:expr, $name:literal $(,)?) => {{
        #[cfg(kernel)]
        let key = ::kernel::static_lock_class!().as_ptr().cast();
        #[cfg(not(kernel))]
        let key = ::core::ptr::null_mut();

        $crate::util::locking::MutexNoio::new(
            $t,
            // SAFETY: NUL-terminated, no NUL before it - a literal's text
            unsafe { ::core::ffi::CStr::from_bytes_with_nul_unchecked(
                ::core::concat!($name, "\0").as_bytes()) },
            key)
    }};
}

pub use crate::new_mutex_noio;

/// mutex_trylock(): the guard if @m wasn't held.
pub fn mutex_trylock(m: &c::mutex) -> Option<MutexGuard<'_>> {
    // SAFETY: as mutex_lock()'s
    let locked = unsafe { c::mutex_trylock(core::ptr::from_ref(m).cast_mut()) };
    // then(), not then_some(): a guard made and dropped unlocks
    (locked as i32 != 0).then(|| MutexGuard(m))
}

/// An SRCU read-side section, open until dropped: srcu_read_lock().
#[must_use = "the SRCU read section ends as soon as the guard is dropped"]
pub struct SrcuReadGuard<'a> {
    srcu: &'a c::srcu_struct,
    idx:  core::ffi::c_int,
}

pub fn srcu_read_lock(srcu: &c::srcu_struct) -> SrcuReadGuard<'_> {
    // SAFETY: an srcu_struct is changed through a shared pointer, as a mutex is
    let idx = unsafe { c::rust_srcu_read_lock(core::ptr::from_ref(srcu).cast_mut()) };
    SrcuReadGuard { srcu, idx }
}

impl Drop for SrcuReadGuard<'_> {
    fn drop(&mut self) {
        unsafe { c::rust_srcu_read_unlock(core::ptr::from_ref(self.srcu).cast_mut(), self.idx) };
    }
}

// ── seqmutex ────────────────────────────────────────────────────────────────
//
// util/seqmutex.h's static inlines: a mutex, and a count of the times it's been
// taken. C reads the count plainly, outside the lock; here it's atomic.

impl c::seqmutex {
    fn seq_atomic(&self) -> &AtomicU32 {
        // SAFETY: a u32, aligned as one, that C writes under the mutex
        unsafe { AtomicU32::from_ptr((&raw const self.seq).cast_mut()) }
    }

    /// seqmutex_lock().
    pub fn lock(&self) -> SeqMutexGuard<'_> {
        let mutex = mutex_lock(&self.lock);
        self.seq_atomic().fetch_add(1, Ordering::Relaxed);
        SeqMutexGuard { lock: self, _mutex: mutex }
    }

    /// seqmutex_relock(): held again, if no one has taken it since the unlock
    /// that gave @seq.
    pub fn relock(&self, seq: SeqMutexSeq) -> Option<SeqMutexGuard<'_>> {
        if self.seq_atomic().load(Ordering::Relaxed) != seq.0 {
            return None;
        }
        let mutex = mutex_trylock(&self.lock)?;
        if self.seq_atomic().load(Ordering::Relaxed) != seq.0 {
            return None;    // unlocked, as the guard drops
        }
        Some(SeqMutexGuard { lock: self, _mutex: mutex })
    }
}

/// A seqmutex, held: unlocked on drop.
#[must_use = "the seqmutex is unlocked as soon as the guard is dropped"]
pub struct SeqMutexGuard<'a> {
    lock:   &'a c::seqmutex,
    _mutex: MutexGuard<'a>,
}

/// What relock() takes: the count at an unlock.
pub struct SeqMutexSeq(u32);

impl SeqMutexGuard<'_> {
    /// seqmutex_unlock().
    pub fn unlock(self) -> SeqMutexSeq {
        SeqMutexSeq(self.lock.seq_atomic().load(Ordering::Relaxed))
    }
}
