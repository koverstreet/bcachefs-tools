// SPDX-License-Identifier: GPL-2.0

//! Locks.
//!
//! A lock is C's lock state - struct mutex, spinlock_t, rw_semaphore,
//! percpu_rw_semaphore - and that's all it is: RawLock (lock.rs) is how one is
//! initialized, taken and released, implemented on C's types themselves
//! (raw.rs), in both builds. Lock<R, G, P> is lock R and the things it
//! protects: G, reached only with it held - its guard derefs to it - and P,
//! published: read by anyone, held or not - the lock derefs to it - and
//! written only by its holder. A plain mutex over a T is Lock<c::mutex, T>;
//! readers share a RawRwLock's, through read(). LockedBy<G, P> is the same two
//! halves where they can't be in the lock: spread through the objects the
//! lock's owner reaches, and reached through it.
//!
//! Noio<R> (noio.rs) is any lock, held in a PF_MEMALLOC_NOIO scope - itself
//! a RawLock, C's struct mutex_noio when R is a mutex. A seqmutex
//! (seqmutex.rs) is a mutex that can be dropped and taken back only if no one
//! took it meanwhile. SeqLock (seqlock.rs) and ByteLock (bytelock.rs) have no
//! C state: atomics - SeqLock's readers copying out, ByteLock a sleeping lock
//! in one byte, on util/futex.rs.
//!
//! compat.rs is what exists because C owns the lock or holds it calling in,
//! and goes as the C does.
//!
//! Initializing a lock in place, new_lock!(), gives it a lockdep class of
//! its own: one per call site, as C's mutex_init() is.
//!
//! The C side: locking.h, C's guards for the noio locks; seqmutex.h, C's
//! seqmutex API; locking.c, the C that Rust calls where C has a macro, or a
//! static inline in one build or the other.

pub mod bytelock;
mod compat;
mod lock;
mod noio;
mod raw;
mod seqlock;
pub mod seqmutex;

pub use crate::new_lock;
pub use bytelock::ByteLock;
pub use raw::{mutex, percpu_rw_semaphore, rw_semaphore, spinlock_t};
pub use lock::{Guard, Lock, LockedBy, RawLock, RawRwLock, RawTryLock, ReadGuard};
pub use noio::{mutex_noio, percpu_rwsem_noio, MemallocFlags, MutexNoio, Noio, PercpuRwsemNoio};
pub use seqlock::SeqLock;
pub use seqmutex::SeqMutexSeq;
