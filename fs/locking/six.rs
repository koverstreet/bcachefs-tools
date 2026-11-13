// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types, non_upper_case_globals)]

#[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
use super::raw::rust_debug_check_no_locks_freed;
use super::lockdep::{trace_contention_begin, trace_contention_end};
use super::{ExclusiveGuard, Locked, LockdepMap, RawLockBase, RawReadLock, RawSixLock, Shared, SharedGuard, SixIntent,
            SixWriteIntent};
use crate::cstructs::c;
use crate::data_structures::dst_vec::DstVec;
use crate::data_structures::inline_vec::InlineVec;
#[cfg(CONFIG_SMP)]
use crate::debug::params::six_optimistic_spin_disabled;
use crate::types::c_default;
use crate::util::alloc::init_at;
use crate::util::ffi::c_char;
use crate::util::kernel::time_after64;
use crate::util::percpu::{CpuGuard, PerCpu};
use crate::util::projections::{AtomicWord, Volatile};
use crate::util::rcu::{self, FreeAnytime};
use crate::util::sched;
use crate::util::sleep::{sleep_until, Sleep};
use core::convert::Infallible;
use core::ffi::{c_int, c_uint, c_ulong, c_void, CStr};
use core::mem::ManuallyDrop;
use core::ptr::{self, null_mut, NonNull};
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use core::sync::atomic::{fence, AtomicBool, AtomicI32, AtomicPtr, AtomicU32};
use cstruct_macros::{bch_enum, CStruct};

#[bch_enum(closed)]
#[repr(u32)]
pub enum SixLockType {
    SIX_LOCK_read,
    SIX_LOCK_intent,
    SIX_LOCK_write,
}

/* C's enum, a u32: zeroes are SIX_LOCK_read, and it's only ever set() to one */
unsafe impl AtomicWord for SixLockType {
    type Atomic = AtomicU32;

    fn load(a: &AtomicU32) -> Self {
        SixLockType::from_bits(a.load(Relaxed))
    }

    fn store(a: &AtomicU32, ty: Self) {
        a.store(ty as u32, Relaxed)
    }
}

/* its own thread's, read by the waker and by other threads' cycle detectors */
#[repr(C)]
#[derive(CStruct)]
pub struct six_lock_waiter {
    pub trans_start_time: Volatile<u64>,
    pub task: AtomicPtr<c::task_struct>,
    pub lock_want: Volatile<c::SixLockType>,
    /* the waker's handoff: set, the waiter may return and reuse this */
    pub lock_acquired: AtomicBool,
    /* Index in the wait list, set on insert, used for O(1) self-remove. */
    pub slot_idx: Volatile<u16>,
}
c_default!(six_lock_waiter);

/* filled under wait_lock, read without it by the cycle detector: @w last */
#[repr(C)]
#[derive(CStruct)]
pub struct six_lock_wait_slot {
    pub w: AtomicPtr<c::six_lock_waiter>,
    pub start_time: Volatile<u64>,
}
c_default!(six_lock_wait_slot);

/* the cycle detector reads them under RCU */
impl !FreeAnytime for six_lock_wait_slot {}

pub const SIX_LOCK_INLINE_WAITERS: u32 = 8;

type WaitList = InlineVec<c::six_lock_wait_slot, { SIX_LOCK_INLINE_WAITERS as usize }, usize>;

/*
 * RCU-swappable wait list. Not a FIFO — entries sit at fixed indices from
 * insertion to removal, so RCU readers (the cycle detector) never observe an
 * entry moving. Removal writes NULL atomically; insertion finds a free slot
 * (starting from @next_free_hint) and fills it. Its length is the
 * high-water-mark index in use and shrinks when trailing slots go tombstone.
 *
 * Grown into a list allocated with wait_lock dropped; what it's grown out of
 * is kvfree_rcu'd, its slots not FreeAnytime, so lockless readers can outrun
 * the free.
 */
#[repr(C)]
#[derive(CStruct)]
pub struct six_lock_waiters {
    pub next_free_hint: u16,
    /* C has no use for it; six_lock_exit() frees it */
    pub list: ManuallyDrop<WaitList>,
}

#[repr(C)]
#[derive(CStruct)]
pub struct six_lock {
    pub state: AtomicI32,
    /* written by the write lock's holder, read without the lock */
    pub seq: Volatile<u32>,
    pub readers: PerCpu<c_uint>,
    pub intent_lock_recurse: Volatile<c_uint>,
    pub write_lock_recurse: Volatile<c_uint>,
    pub owner: AtomicPtr<c::task_struct>,
    /* the intent lock's holder's */
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub owner_stack: core::cell::UnsafeCell<crate::util::alloc::KVVec<c_ulong>>,
    pub wait_lock: Locked<c::raw_spinlock_t, six_lock_waiters>,
    pub dep_map: LockdepMap,
}

#[bch_enum(flags)]
#[repr(u32)]
pub enum SixLockInitFlags {
    SIX_LOCK_INIT_PCPU = 1 << 0,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct six_lock_count {
    pub n: [c_uint; 3],
}

/*
 * Wait list entry: cached {waiter pointer, start_time + lock_want} pair.
 * Keeping start_time and lock_want in the slot lets the wakeup scan filter
 * and pick the oldest matching waiter without dereffing each six_lock_waiter
 * — only the chosen one is touched via @w.
 *
 * Low SIX_LOCK_WANT_BITS bits of @start_time hold lock_want; the remaining
 * high bits hold trans_start_time shifted up. Time comparisons among waiters
 * of the same lock_want are unaffected — the low bits are equal — so callers
 * can compare @start_time directly without masking.
 */
pub const SIX_LOCK_WANT_BITS: u32 = 2;
pub const SIX_LOCK_WANT_MASK: u32 = (1 << SIX_LOCK_WANT_BITS) - 1;

const SIX_LOCK_HELD_read_OFFSET: u32 = 0;
const SIX_LOCK_HELD_read: u32 = !(!0u32 << 26);
const SIX_LOCK_HELD_intent: u32 = 1 << 26;
const SIX_LOCK_HELD_write: u32 = 1 << 27;
const SIX_LOCK_WAITING_read: u32 = 1 << (28 + (SixLockType::SIX_LOCK_read as u32));
const SIX_LOCK_WAITING_write: u32 = 1 << (28 + (SixLockType::SIX_LOCK_write as u32));
const SIX_LOCK_NOSPIN: u32 = 1 << 31;

struct six_lock_vals {
    /* Value we add to the lock in order to take the lock: */
    lock_val:      u32,

    /* If the lock has this value (used as a mask), taking the lock fails: */
    lock_fail:     u32,

    /* Mask that indicates lock is held for this type: */
    held_mask:     u32,

    /* Waitlist we wakeup when releasing the lock: */
    unlock_wakeup: SixLockType,
}

static L: [six_lock_vals; 3] = [
    six_lock_vals {
        lock_val:      1 << SIX_LOCK_HELD_read_OFFSET,
        lock_fail:     SIX_LOCK_HELD_write,
        held_mask:     SIX_LOCK_HELD_read,
        unlock_wakeup: SixLockType::SIX_LOCK_write,
    },
    six_lock_vals {
        lock_val:      SIX_LOCK_HELD_intent,
        lock_fail:     SIX_LOCK_HELD_intent,
        held_mask:     SIX_LOCK_HELD_intent,
        unlock_wakeup: SixLockType::SIX_LOCK_intent,
    },
    six_lock_vals {
        lock_val:      SIX_LOCK_HELD_write,
        lock_fail:     SIX_LOCK_HELD_read,
        held_mask:     SIX_LOCK_HELD_write,
        unlock_wakeup: SixLockType::SIX_LOCK_read,
    },
];

#[inline(always)]
fn atomic_read(lock: &six_lock) -> u32 {
    lock.state.load(Relaxed) as u32
}

#[inline(always)]
fn six_set_bitmask(lock: &six_lock, mask: u32) {
    if atomic_read(lock) & mask != mask {
        lock.state.fetch_or(mask as i32, Relaxed);
    }
}

#[inline(always)]
fn six_clear_bitmask(lock: &six_lock, mask: u32) {
    if atomic_read(lock) & mask != 0 {
        lock.state.fetch_and(!mask as i32, Relaxed);
    }
}

#[inline(always)]
fn six_set_owner(lock: &six_lock, ty: SixLockType, old: u32, owner: *mut c::task_struct) {
    if ty != SixLockType::SIX_LOCK_intent {
        return;
    }

    if old & SIX_LOCK_HELD_intent == 0 {
        ebug_assert!(lock.owner.load(Relaxed).is_null());
        lock.owner.store(owner, Relaxed);
    } else {
        ebug_assert!(lock.owner.load(Relaxed) == sched::current());
    }
}

fn six_lock_record_acquire(lock: &six_lock, ty: SixLockType) {
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    if ty == SixLockType::SIX_LOCK_intent {
        // a trylock's caller may hold a C spinlock, which isn't a scope
        let _noreclaim = super::MemallocFlags::noreclaim();
        let _ = unsafe {
            crate::util::backtrace::save_backtrace(&mut *lock.owner_stack.get(), sched::current(), 1)
        };
    }
    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    let _ = (lock, ty);
}

#[inline(always)]
fn pcpu_read_count(lock: &six_lock) -> u32 {
    lock.readers.get().sum()
}

/// __do_six_trylock()'s result.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trylock {
    Failed,
    Acquired,
    /// Failed, and owes a wakeup of this type.
    Wakeup(SixLockType),
}

/*
 * __do_six_trylock() - main trylock routine
 *
 * Returns Acquired on success, Failed on failure
 *
 * In percpu reader mode, a failed trylock may cause a spurious trylock failure
 * for another thread taking the competing lock type, and we may have to do a
 * wakeup: when a wakeup is required, we return Wakeup(wakeup_type).
 */
#[inline(always)]
fn __do_six_trylock(lock: &six_lock, ty: SixLockType, task: *mut c::task_struct, try_: bool) -> Trylock {
    use SixLockType::*;
    let l = &L[ty as usize];
    let mut old = 0;

    ebug_assert!(ty != SIX_LOCK_write || lock.owner.load(Relaxed) == task);
    ebug_assert!(ty != SIX_LOCK_write ||
                 try_ == (atomic_read(lock) & SIX_LOCK_HELD_write == 0));

    /*
     * Percpu reader mode:
     *
     * The basic idea behind this algorithm is that you can implement a lock
     * between two threads without any atomics, just memory barriers:
     *
     * For two threads you'll need two variables, one variable for "thread a
     * has the lock" and another for "thread b has the lock".
     *
     * To take the lock, a thread sets its variable indicating that it holds
     * the lock, then issues a full memory barrier, then reads from the
     * other thread's variable to check if the other thread thinks it has
     * the lock. If we raced, we backoff and retry/sleep.
     *
     * Failure to take the lock may cause a spurious trylock failure in
     * another thread, because we temporarily set the lock to indicate that
     * we held it. This would be a problem for a thread in six_lock(), when
     * they are calling trylock after adding themself to the waitlist and
     * prior to sleeping.
     *
     * Therefore, if we fail to get the lock, and there were waiters of the
     * type we conflict with, we will have to issue a wakeup.
     *
     * Since we may be called under wait_lock (and by the wakeup code
     * itself), we return that the wakeup has to be done instead of doing it
     * here.
     */
    let ret = if ty == SIX_LOCK_intent || !lock.readers.allocated() {
        old = atomic_read(lock);
        let ret = loop {
            let ret = old & l.lock_fail == 0;
            if !ret || (ty == SIX_LOCK_write && !try_) {
                fence(SeqCst);
                break ret;
            }
            match lock.state.compare_exchange_weak(old as i32, old.wrapping_add(l.lock_val) as i32,
                                                   Acquire, Relaxed) {
                Ok(_)    => break true,
                Err(cur) => old = cur as u32,
            }
        };

        ebug_assert!(!ret || atomic_read(lock) & l.held_mask != 0);
        if ret { Trylock::Acquired } else { Trylock::Failed }
    } else if ty == SIX_LOCK_read {
        let _g = CpuGuard::new();
        let readers = lock.readers.get();

        readers.add(1); /* signal that we own lock */

        fence(SeqCst);

        old = atomic_read(lock);
        if old & l.lock_fail == 0 {
            Trylock::Acquired
        } else {
            readers.add(1u32.wrapping_neg());
            fence(SeqCst);
            if atomic_read(lock) & SIX_LOCK_WAITING_write != 0 {
                Trylock::Wakeup(SIX_LOCK_write)
            } else {
                Trylock::Failed
            }
        }
    } else {
        if try_ {
            lock.state.fetch_add(SIX_LOCK_HELD_write as i32, Relaxed);
        }

        /*
         * Make sure atomic_add happens before pcpu_read_count and
         * six_set_bitmask in slow path happens before pcpu_read_count.
         *
         * Paired with the smp_mb() in read lock fast path (per-cpu mode)
         * and the one before atomic_read in read unlock path.
         */
        fence(SeqCst);
        let ret = pcpu_read_count(lock) == 0;

        if try_ && !ret {
            /* atomic_sub_return(): fully ordered */
            fence(SeqCst);
            old = (lock.state.fetch_sub(SIX_LOCK_HELD_write as i32, Relaxed) as u32)
                .wrapping_sub(SIX_LOCK_HELD_write);
            fence(SeqCst);
            if old & SIX_LOCK_WAITING_read != 0 {
                Trylock::Wakeup(SIX_LOCK_read)
            } else {
                Trylock::Failed
            }
        } else if ret {
            Trylock::Acquired
        } else {
            Trylock::Failed
        }
    };

    if ret == Trylock::Acquired {
        six_set_owner(lock, ty, old, task);
    }

    ebug_assert!(!(ty == SIX_LOCK_write && try_ && ret != Trylock::Acquired &&
                   atomic_read(lock) & SIX_LOCK_HELD_write != 0));

    ret
}

type WaitLockGuard<'a> = ExclusiveGuard<'a, c::raw_spinlock_t, six_lock_waiters>;

/// The wait list, as wait_lock's holder has it, and its lock: what fills and
/// empties its slots.
struct WaitListLocked<'a> {
    lock: &'a six_lock,
    w:    &'a mut six_lock_waiters,
}

impl six_lock {
    /// The wait list, for @g, wait_lock's holder.
    fn wait_list<'a>(&'a self, g: &'a mut WaitLockGuard<'_>) -> WaitListLocked<'a> {
        ebug_assert!(ptr::eq(g.lock(), &self.wait_lock));
        WaitListLocked { lock: self, w: g }
    }

    /// The wait list's slots, read without wait_lock.
    fn wait_list_rcu<'a>(&'a self, rcu: &'a rcu::ReadGuard) -> &'a [six_lock_wait_slot] {
        // wait_lock's holder has it by &mut, and readers by read_rcu() alone
        unsafe { WaitList::read_rcu((&raw const (*self.wait_lock.data_ptr()).list).cast(), rcu) }
    }

    /// Its waiters and the lock type each wants, read without wait_lock: for
    /// the cycle detector. A slot's waiter is read with acquire, before its
    /// start_time - the publish in WaitListLocked::insert() is a release.
    pub fn waiters<'a>(&'a self, rcu: &'a rcu::ReadGuard)
        -> impl Iterator<Item = (NonNull<six_lock_waiter>, SixLockType)> + 'a
    {
        self.wait_list_rcu(rcu).iter().filter_map(|slot| {
            let w = NonNull::new(slot.w.load(Acquire))?;
            Some((w, SixLockType::from_bits((slot.start_time.get() & SIX_LOCK_WANT_MASK as u64) as u32)))
        })
    }
}

impl WaitListLocked<'_> {
    /*
     * Trim @nr while the tail is all tombstones. Clamp hint inside [0, nr].
     */
    #[inline(always)]
    fn shrink(&mut self) {
        let list = &mut *self.w.list;
        let mut nr = list.len();

        while nr > 0 && list[nr - 1].w.load(Relaxed).is_null() {
            nr -= 1;
        }
        list.truncate(nr);
        self.w.next_free_hint = self.w.next_free_hint.min(nr as u16);
    }

    /*
     * Null out the entry at @idx and update the free-slot hint. Does not
     * shrink @nr — that's done lazily by the caller when convenient (e.g. at
     * the end of a wakeup scan).
     */
    #[inline(always)]
    fn remove(&mut self, idx: usize) {
        /*
         * WRITE_ONCE: the lockless cycle detector reads this slot via
         * smp_load_acquire(&.w); clearing to NULL just makes it skip the slot
         * (no dependent data, so no release needed), but it's a concurrently
         * read shared location, so the store must be marked.
         */
        self.w.list[idx].w.store(null_mut(), Relaxed);
        self.w.next_free_hint = self.w.next_free_hint.min(idx as u16);
    }

    /*
     * Find a free slot (starting from @next_free_hint) and write the entry.
     * Returns true on success, false if the list is full.
     */
    /// # Safety
    /// @wait lives until it's removed.
    #[inline(always)]
    unsafe fn insert(&mut self, wait: &six_lock_waiter) -> bool {
        let list = &mut *self.w.list;
        let (nr, size) = (list.len(), list.capacity());
        let free = |i: usize| i >= nr || list[i].w.load(Relaxed).is_null();

        let hint = self.w.next_free_hint as usize;
        let i = if free(hint) {
            hint
        } else if let Some(i) = (0..nr).find(|&i| free(i)) {
            i
        } else if nr < size {
            nr
        } else {
            return false;
        };
        if i == nr && list.push(six_lock_wait_slot::default()).is_err() {
            return false;
        }

        /*
         * Publish this waiter for the lockless cycle detector
         * (bch2_check_for_deadlock), which walks this fifo without taking
         * wait_lock, gates each slot on a non-NULL .w, then reads the sibling
         * .start_time and follows container_of(.w) -> trans to read the
         * waiter's held-lock state.
         *
         * .w is the publish gate: stored last, with a release that orders the
         * sibling slot fields and all of the trans's held-lock state (->paths,
         * ->nodes_locked, l[].b) before the slot becomes visible. Pairs with
         * the smp_load_acquire(&i->w) in the walk.
         *
         * This release is *not* the store->load fence that lets two waiters
         * closing a cycle see each other - that is the smp_mb() in
         * bch2_check_for_deadlock.
         */
        let slot = &list[i];
        slot.start_time.set((wait.trans_start_time.get() << SIX_LOCK_WANT_BITS) |
                            (wait.lock_want.get() as u64 & SIX_LOCK_WANT_MASK as u64));
        wait.slot_idx.set(i as u16);
        slot.w.store(ptr::from_ref(wait).cast_mut(), Release);
        self.w.next_free_hint = ((i + 1) & (size - 1)) as u16;
        true
    }

    /// The task of the waiter in slot @i.
    fn task(&self, i: usize) -> *mut c::task_struct {
        // inserted, so it lives until it's handed the lock or removes itself
        unsafe { (*self.w.list[i].w.load(Relaxed)).task.load(Relaxed) }
    }

    /// Hand the waiter in slot @i the lock: removed, then lock_acquired, after
    /// which it may return - so its task is had before, and woken after.
    fn wake(&mut self, i: usize) {
        let w = self.w.list[i].w.load(Relaxed);
        let task = self.task(i);

        /*
         * Similar to percpu_rwsem_wake_function(), we need to
         * guard against the wakee noticing w->lock_acquired,
         * returning, and then exiting before we do the wakeup:
         */
        unsafe {
            c::get_task_struct(task);
            self.remove(i);
            /*
             * Release barrier orders the slot clear before
             * setting w->lock_acquired; @w is on the stack of the
             * waiting thread and will be reused after it sees
             * w->lock_acquired with no other locking: pairs with
             * smp_load_acquire() in six_lock_slowpath()
             */
            (*w).lock_acquired.store(true, Release);
            c::wake_up_process(task);
            c::put_task_struct(task);
        }
    }
}

/*
 * Wake the single matching waiter with the smallest start_time (oldest
 * transaction). On release, only that slot is nulled — the rest of the list
 * remains stable from the lockless cycle detector's perspective.
 *
 * For reads: wake all matching waiters, same start-time ordering isn't
 * relevant since readers don't conflict with each other.
 */
fn __six_lock_wakeup(mut wl: WaitListLocked<'_>, mut lock_type: SixLockType) {
    let lock = wl.lock;
    let matches = |slot: &six_lock_wait_slot, ty: SixLockType|
        !slot.w.load(Relaxed).is_null() && slot.start_time.get() & SIX_LOCK_WANT_MASK as u64 == ty as u64;

    loop {
        let ret = 'out: {
            if lock_type == SixLockType::SIX_LOCK_read {
                /* Readers don't conflict: wake all matching waiters. */
                for i in 0..wl.w.list.len() {
                    if !matches(&wl.w.list[i], lock_type) {
                        continue;
                    }

                    let ret = __do_six_trylock(lock, lock_type, wl.task(i), false);
                    if ret != Trylock::Acquired {
                        break 'out ret;
                    }

                    wl.wake(i);
                }
            } else {
                /*
                 * Intent/write locks have a single holder: prefer the waiter
                 * whose transaction started earliest. start_time is cached in
                 * the slot so the scan doesn't deref each waiter.
                 */
                let mut oldest: Option<(usize, u64)> = None;
                let mut n_matches = 0;

                for (i, slot) in wl.w.list.iter().enumerate() {
                    if !matches(slot, lock_type) {
                        continue;
                    }
                    n_matches += 1;
                    let start_time = slot.start_time.get();
                    if oldest.is_none_or(|(_, oldest)| time_after64(oldest, start_time)) {
                        oldest = Some((i, start_time));
                    }
                }

                if let Some((i, _)) = oldest {
                    let ret = __do_six_trylock(lock, lock_type, wl.task(i), false);
                    if ret != Trylock::Acquired {
                        break 'out ret;
                    }

                    wl.wake(i);

                    /*
                     * More non-read waiters still queued: leave the
                     * WAITING bit set so the next unlock re-enters
                     * __six_lock_wakeup and serves them.
                     */
                    if n_matches > 1 {
                        break 'out Trylock::Acquired;
                    }
                }
            }

            six_clear_bitmask(lock, SIX_LOCK_WAITING_read << lock_type as u32);
            Trylock::Acquired
        };

        match ret {
            Trylock::Wakeup(ty) => lock_type = ty,
            _                   => break,
        }
    }

    wl.shrink();
}

#[inline(always)]
fn six_lock_wakeup(lock: &six_lock, state: u32, lock_type: SixLockType) {
    if lock_type == SixLockType::SIX_LOCK_write && state & SIX_LOCK_HELD_read != 0 {
        return;
    }

    if state & (SIX_LOCK_WAITING_read << lock_type as u32) == 0 {
        return;
    }

    let mut g = lock.wait_lock.lock();
    __six_lock_wakeup(lock.wait_list(&mut g), lock_type);
}

#[inline(always)]
fn do_six_trylock(lock: &six_lock, ty: SixLockType, try_: bool) -> bool {
    let ret = __do_six_trylock(lock, ty, sched::current(), try_);

    if let Trylock::Wakeup(wakeup) = ret {
        let mut g = lock.wait_lock.lock();
        __six_lock_wakeup(lock.wait_list(&mut g), wakeup);
    }

    ret == Trylock::Acquired
}

/*
 * six_trylock_ip - attempt to take a six lock without blocking
 * @lock:	lock to take
 * @type:	SIX_LOCK_read, SIX_LOCK_intent, or SIX_LOCK_write
 * @ip:		ip parameter for lockdep/lockstat, i.e. _THIS_IP_
 *
 * Return: true on success, false on failure.
 */
pub fn six_trylock_ip(lock: &c::six_lock, ty: c::SixLockType, ip: c_ulong) -> bool {
    if !do_six_trylock(lock, ty, true) {
        return false;
    }

    if ty != SixLockType::SIX_LOCK_write {
        lock.dep_map.acquire(true, ty == SixLockType::SIX_LOCK_read, ip);
    }
    six_lock_record_acquire(lock, ty);
    true
}

/*
 * six_relock_ip - attempt to re-take a lock that was held previously
 * @lock:	lock to take
 * @type:	SIX_LOCK_read, SIX_LOCK_intent, or SIX_LOCK_write
 * @seq:	lock sequence number obtained from six_lock_seq() while lock was
 *		held previously
 * @ip:		ip parameter for lockdep/lockstat, i.e. _THIS_IP_
 *
 * Return: true on success, false on failure.
 */
pub fn six_relock_ip(lock: &c::six_lock, ty: c::SixLockType, seq: c_uint, ip: c_ulong) -> bool {
    if lock.seq.get() != seq || !six_trylock_ip(lock, ty, ip) {
        return false;
    }

    if lock.seq.get() != seq {
        unsafe { six_unlock_ip(lock, ty, ip) };
        return false;
    }

    true
}

#[cfg(CONFIG_SMP)]
fn six_owner_running(lock: &six_lock) -> bool {
    /*
     * When there's no owner, we might have preempted between the owner
     * acquiring the lock and setting the owner field. If we're an RT task
     * that will live-lock because we won't let the owner complete.
     */
    let rcu = rcu::read_lock();
    match NonNull::new(lock.owner.load(Relaxed)) {
        Some(owner) => sched::owner_on_cpu(owner, &rcu),
        None        => !sched::rt_or_dl_task_current(),
    }
}

#[cfg(CONFIG_SMP)]
fn six_optimistic_spin(lock: &six_lock, wait: &six_lock_waiter, ty: SixLockType) -> bool {
    if ty == SixLockType::SIX_LOCK_write || six_optimistic_spin_disabled() {
        return false;
    }

    {
        let rcu = rcu::read_lock();
        let wl = lock.wait_list_rcu(&rcu);
        /*
         * Only spin if we're the sole entry in the wait list. Anything
         * more and someone older might get woken ahead of us.
         */
        if wl.len() != 1 || !ptr::eq(wl[0].w.load(Relaxed), wait) {
            return false;
        }
    }

    if atomic_read(lock) & SIX_LOCK_NOSPIN != 0 {
        return false;
    }

    let _g = CpuGuard::new();
    let end_time = sched::sched_clock() + 10 * 1000;
    let mut loop_: u32 = 0;

    while !sched::need_resched() && six_owner_running(lock) {
        /*
         * Ensures that writes to the waitlist entry happen after we see
         * wait->lock_acquired: pairs with the smp_store_release in
         * __six_lock_wakeup
         */
        if wait.lock_acquired.load(Acquire) {
            return true;
        }

        loop_ = loop_.wrapping_add(1);
        if loop_ & 0xf == 0 && time_after64(sched::sched_clock(), end_time) {
            six_set_bitmask(lock, SIX_LOCK_NOSPIN);
            break;
        }

        /*
         * The cpu_relax() call is a compiler barrier which forces
         * everything in this loop to be re-loaded. We don't need
         * memory barriers as we'll eventually observe the right
         * values at the cost of a few extra spins.
         */
        core::hint::spin_loop();
    }

    false
}

#[cfg(not(CONFIG_SMP))]
fn six_optimistic_spin(_lock: &six_lock, _wait: &six_lock_waiter, _ty: SixLockType) -> bool {
    false
}

#[inline(always)]
fn __six_lock_slowpath(lock: &six_lock, ty: SixLockType, wait: &six_lock_waiter,
                       mut should_sleep_fn: impl FnMut() -> c_int, ip: c_ulong) -> c_int {
    use SixLockType::*;
    let mut spare = None;
    let mut ret: c_int = 0;

    if ty == SIX_LOCK_write {
        ebug_assert!(atomic_read(lock) & SIX_LOCK_HELD_write == 0);
        lock.state.fetch_add(SIX_LOCK_HELD_write as i32, Relaxed);
        /* smp_mb__after_atomic() */
        fence(SeqCst);
    }

    trace_contention_begin(lock, 0);
    lock.dep_map.contended(ip);

    wait.task.store(sched::current(), Relaxed);
    wait.lock_want.set(ty);
    wait.lock_acquired.store(false, Relaxed);

    'out: {
        let acquired = loop {
            let mut g = lock.wait_lock.lock();

            /*
             * Retry taking the lock after taking waitlist lock, in case we raced
             * with an unlock:
             */
            six_set_bitmask(lock, SIX_LOCK_WAITING_read << ty as u32);
            match __do_six_trylock(lock, ty, sched::current(), false) {
                Trylock::Acquired       => break true,
                Trylock::Failed         => {}
                Trylock::Wakeup(wakeup) => __six_lock_wakeup(lock.wait_list(&mut g), wakeup),
            }

            // nothing below returns with @wait still on the list: it's
            // handed the lock, or removes itself
            let mut wl = lock.wait_list(&mut g);
            if unsafe { wl.insert(wait) } {
                break false;
            }
            if let Some(s) = spare.take() {
                // a spare no bigger - grown meanwhile - is handed back, and freed
                if wl.w.list.grow_into(s).is_ok() {
                    let inserted = unsafe { wl.insert(wait) };
                    ebug_assert!(inserted);
                    break false;
                }
            }

            /* full: one twice the size, allocated with wait_lock dropped */
            let size = wl.w.list.capacity();
            drop(g);

            spare = if size < 1 << 15 { DstVec::with_capacity(size * 2).ok() } else { None };
            if spare.is_none() {
                if ty == SIX_LOCK_write {
                    six_clear_bitmask(lock, SIX_LOCK_HELD_write);
                    six_lock_wakeup(lock, atomic_read(lock), SIX_LOCK_read);
                }
                ret = -(c::ENOMEM as c_int);
                break 'out;
            }
        };

        if acquired {
            break 'out;
        }

        if six_optimistic_spin(lock, wait, ty) || wait.lock_acquired.load(Acquire) {
            break 'out;
        }

        /* Yield before running the cycle detector: */
        sched::schedule();

        let Ok(r) = sleep_until(Sleep::UNINTERRUPTIBLE, || {
            /*
             * Ensures that writes to the waitlist entry happen after we see
             * wait->lock_acquired: pairs with the smp_store_release in
             * __six_lock_wakeup
             */
            if wait.lock_acquired.load(Acquire) {
                return Some(0);
            }

            match should_sleep_fn() {
                0 => None,
                e => Some(e),
            }
        });
        ret = r;

        if ret != 0 {
            /*
             * If should_sleep_fn() returns an error, we are
             * required to return that error even if we already
             * acquired the lock - should_sleep_fn() might have
             * modified external state (e.g. when the deadlock cycle
             * detector in bcachefs issued a transaction restart)
             */
            let acquired = {
                let mut g = lock.wait_lock.lock();
                let acquired = wait.lock_acquired.load(Relaxed);
                if !acquired {
                    let mut wl = lock.wait_list(&mut g);

                    wl.remove(wait.slot_idx.get().into());
                    wl.shrink();
                }
                acquired
            };

            if acquired {
                do_six_unlock_type(lock, ty);
            } else if ty == SIX_LOCK_write {
                six_clear_bitmask(lock, SIX_LOCK_HELD_write);
                six_lock_wakeup(lock, atomic_read(lock), SIX_LOCK_read);
            }
        }
    }

    trace_contention_end(lock, 0);

    ret
}

#[inline(never)]
fn six_lock_slowpath(lock: &six_lock, ty: SixLockType, wait: &six_lock_waiter,
                     should_sleep_fn: impl FnMut() -> c_int, ip: c_ulong) -> c_int {
    __six_lock_slowpath(lock, ty, wait, should_sleep_fn, ip)
}

/*
 * six_lock_ip_waiter - take a lock, with full waitlist interface
 * @lock:	lock to take
 * @type:	SIX_LOCK_read, SIX_LOCK_intent, or SIX_LOCK_write
 * @wait:	pointer to wait object, which will be added to lock's waitlist
 * @should_sleep_fn: callback run after adding to waitlist, immediately prior
 *		to scheduling
 * @p:		passed through to @should_sleep_fn
 * @ip:		ip parameter for lockdep/lockstat, i.e. _THIS_IP_
 *
 * This is the most general six_lock() variant, with parameters to support full
 * cycle detection for deadlock avoidance.
 *
 * The code calling this function must implement tracking of held locks, and the
 * @wait object should be embedded into the struct that tracks held locks -
 * which must also be accessible in a thread-safe way.
 *
 * @should_sleep_fn should invoke the cycle detector; it should walk each
 * lock's waiters, and for each waiter recursively walk their held locks.
 *
 * When this function must block, @wait will be added to @lock's waitlist before
 * calling trylock, and before calling @should_sleep_fn, and @wait will not be
 * removed from the lock waitlist until the lock has been successfully acquired,
 * or we abort.
 *
 * @wait.trans_start_time orders waiters on the same waitlist (oldest waiter
 * wins), and may be used as a loop cursor for cycle detection.
 *
 * Return: 0 on success, or the return code from @should_sleep_fn on failure.
 */
pub fn six_lock_ip_waiter(lock: &c::six_lock, ty: c::SixLockType, wait: &c::six_lock_waiter,
                          should_sleep_fn: impl FnMut() -> c_int, ip: c_ulong) -> c_int {
    if ty != SixLockType::SIX_LOCK_write {
        lock.dep_map.acquire(false, ty == SixLockType::SIX_LOCK_read, ip);
    }

    let ret = if do_six_trylock(lock, ty, true) {
        0
    } else {
        six_lock_slowpath(lock, ty, wait, should_sleep_fn, ip)
    };

    if ret != 0 && ty != SixLockType::SIX_LOCK_write {
        lock.dep_map.release(ip);
    }
    if ret == 0 {
        lock.dep_map.acquired(ip);
        six_lock_record_acquire(lock, ty);
    }

    ret
}

/*
 * As six_lock_ip_waiter(), but skip the initial trylock and go straight
 * to the waitlist slowpath. Use this when the caller has already done a
 * trylock and observed contention (e.g. btree_node_lock dispatching
 * to btree_node_lock_nopath after its own fast-path trylock failed) —
 * avoids burning a second locked CAS on a lock state we already know
 * is contended.
 */
/*
 * As six_lock_ip_waiter() but skip the initial trylock. The caller has
 * already attempted (and observed failure) — burning another locked CAS
 * to recheck would be wasted work. The post-WAITING-bit retry inside
 * six_lock_slowpath() still covers the unlock-raced-with-us window.
 */
pub fn six_lock_contended(lock: &c::six_lock, ty: c::SixLockType, wait: &c::six_lock_waiter,
                          should_sleep_fn: impl FnMut() -> c_int, ip: c_ulong) -> c_int {
    if ty != SixLockType::SIX_LOCK_write {
        lock.dep_map.acquire(false, ty == SixLockType::SIX_LOCK_read, ip);
    }

    let ret = __six_lock_slowpath(lock, ty, wait, should_sleep_fn, ip);

    if ret != 0 && ty != SixLockType::SIX_LOCK_write {
        lock.dep_map.release(ip);
    }
    if ret == 0 {
        lock.dep_map.acquired(ip);
        six_lock_record_acquire(lock, ty);
    }

    ret
}

#[inline(always)]
fn do_six_unlock_type(lock: &six_lock, ty: SixLockType) {
    let l = &L[ty as usize];
    let state;

    if ty == SixLockType::SIX_LOCK_intent {
        lock.owner.store(null_mut(), Relaxed);
    }

    if ty == SixLockType::SIX_LOCK_read && lock.readers.allocated() {
        fence(SeqCst); /* unlock barrier */
        lock.readers.get().add(1u32.wrapping_neg());
        fence(SeqCst); /* between unlocking and checking for waiters */
        state = atomic_read(lock);
    } else {
        let mut v = l.lock_val;

        if ty != SixLockType::SIX_LOCK_read {
            v += atomic_read(lock) & SIX_LOCK_NOSPIN;
        }

        ebug_assert!(atomic_read(lock) & l.held_mask != 0);
        state = (lock.state.fetch_sub(v as i32, Release) as u32).wrapping_sub(v);
    }

    six_lock_wakeup(lock, state, l.unlock_wakeup);
}

/*
 * six_unlock_ip - drop a six lock
 * @lock:	lock to unlock
 * @type:	SIX_LOCK_read, SIX_LOCK_intent, or SIX_LOCK_write
 * @ip:		ip parameter for lockdep/lockstat, i.e. _THIS_IP_
 *
 * When a lock is held multiple times (because six_lock_incement()) was used),
 * this decrements the 'lock held' counter by one.
 *
 * For example:
 * six_lock_read(&foo->lock);				read count 1
 * six_lock_increment(&foo->lock, SIX_LOCK_read);	read count 2
 * six_lock_unlock(&foo->lock, SIX_LOCK_read);		read count 1
 * six_lock_unlock(&foo->lock, SIX_LOCK_read);		read count 0
 */
/// # Safety
/// @lock is held as @ty, by the caller.
pub unsafe fn six_unlock_ip(lock: &c::six_lock, ty: c::SixLockType, ip: c_ulong) {
    use SixLockType::*;

    ebug_assert!(!(ty == SIX_LOCK_write && atomic_read(lock) & SIX_LOCK_HELD_intent == 0));
    ebug_assert!(!((ty == SIX_LOCK_write || ty == SIX_LOCK_intent) &&
                   lock.owner.load(Relaxed) != sched::current()));

    if ty != SIX_LOCK_write {
        lock.dep_map.release(ip);
    }

    if ty == SIX_LOCK_intent && lock.intent_lock_recurse.get() != 0 {
        lock.intent_lock_recurse.set(lock.intent_lock_recurse.get() - 1);
        return;
    }

    if ty == SIX_LOCK_write && lock.write_lock_recurse.get() != 0 {
        lock.write_lock_recurse.set(lock.write_lock_recurse.get() - 1);
        return;
    }

    if ty == SIX_LOCK_write {
        lock.seq.set(lock.seq.get().wrapping_add(1));
    }

    do_six_unlock_type(lock, ty);
}

/*
 * six_lock_downgrade - convert an intent lock to a read lock
 * @lock:	lock to dowgrade
 *
 * @lock will have read count incremented and intent count decremented
 */
/// # Safety
/// @lock is held for intent, by the caller.
pub unsafe fn six_lock_downgrade(lock: &c::six_lock) {
    let intent = unsafe { SixHold::from_raw(Locked::from_c(lock), SixLockType::SIX_LOCK_intent) };
    intent.downgrade().into_raw();
}

/*
 * six_lock_tryupgrade - attempt to convert read lock to an intent lock
 * @lock:	lock to upgrade
 *
 * On success, @lock will have intent count incremented and read count
 * decremented
 *
 * Return: true on success, false on failure
 */
/// # Safety
/// @lock is held for read, by the caller.
pub unsafe fn six_lock_tryupgrade(lock: &c::six_lock) -> bool {
    let read = unsafe { SixHold::from_raw(Locked::from_c(lock), SixLockType::SIX_LOCK_read) };
    let (Ok(hold) | Err(hold)) = read.try_upgrade();
    hold.into_raw() == SixLockType::SIX_LOCK_intent
}

/*
 * six_trylock_convert - attempt to convert a held lock from one type to another
 * @lock:	lock to upgrade
 * @from:	SIX_LOCK_read or SIX_LOCK_intent
 * @to:		SIX_LOCK_read or SIX_LOCK_intent
 *
 * On success, @lock will have intent count incremented and read count
 * decremented
 *
 * Return: true on success, false on failure
 */

/*
 * six_lock_increment - increase held lock count on a lock that is already held
 * @lock:	lock to increment
 * @type:	SIX_LOCK_read or SIX_LOCK_intent
 *
 * @lock must already be held, with a lock type that is greater than or equal to
 * @type
 *
 * A corresponding six_unlock_type() call will be required for @lock to be fully
 * unlocked.
 */
/// # Safety
/// @lock is held as @ty or stronger, by the caller.
pub unsafe fn six_lock_increment_ip(lock: &c::six_lock, ty: c::SixLockType, ip: c_ulong) {
    use SixLockType::*;

    lock.dep_map.acquire(false, ty == SIX_LOCK_read, ip);

    /* XXX: assert already locked, and that we don't overflow: */

    match ty {
        SIX_LOCK_read => {
            if lock.readers.allocated() {
                lock.readers.get().add(1);
            } else {
                ebug_assert!(atomic_read(lock) & (SIX_LOCK_HELD_read | SIX_LOCK_HELD_intent) != 0);
                lock.state.fetch_add(L[ty as usize].lock_val as i32, Relaxed);
            }
        }
        SIX_LOCK_write | SIX_LOCK_intent => {
            if ty == SIX_LOCK_write {
                lock.write_lock_recurse.set(lock.write_lock_recurse.get() + 1);
            }
            ebug_assert!(atomic_read(lock) & SIX_LOCK_HELD_intent != 0);
            lock.intent_lock_recurse.set(lock.intent_lock_recurse.get() + 1);
        }
    }
}

/*
 * six_lock_wakeup_all - wake up all waiters on @lock
 * @lock:	lock to wake up waiters for
 *
 * Wakeing up waiters will cause them to re-run should_sleep_fn, which may then
 * abort the lock operation.
 *
 * This function is never needed in a bug-free program; it's only useful in
 * debug code, e.g. to determine if a cycle detector is at fault.
 */
pub fn six_lock_wakeup_all(lock: &c::six_lock) {
    let state = atomic_read(lock);

    /*
     * First, run the normal wakeup machinery for any waiters the lock
     * state actually permits to acquire: those slots get removed by
     * __six_lock_wakeup and lock_acquired is set on the waiter.
     */
    six_lock_wakeup(lock, state, SixLockType::SIX_LOCK_read);
    six_lock_wakeup(lock, state, SixLockType::SIX_LOCK_intent);
    six_lock_wakeup(lock, state, SixLockType::SIX_LOCK_write);

    /*
     * Then wake any remaining waiters without removing their slots:
     * lock_acquired stays clear, so they'll re-run should_sleep_fn in
     * six_lock_slowpath() and self-remove via wait->slot_idx if it
     * returns an error. Removing here would corrupt that self-remove.
     */
    let mut g = lock.wait_lock.lock();
    let wl = lock.wait_list(&mut g);
    for i in 0..wl.w.list.len() {
        if !wl.w.list[i].w.load(Relaxed).is_null() {
            unsafe { c::wake_up_process(wl.task(i)) };
        }
    }
}

/*
 * six_lock_counts - return held lock counts, for each lock type
 * @lock:	lock to return counters for
 *
 * Return: the number of times a lock is held for read, intent and write.
 */
/// Whether it's held for write: by its intent holder, if anyone.
pub fn six_lock_write_held(lock: &c::six_lock) -> bool {
    atomic_read(lock) & SIX_LOCK_HELD_write != 0
}

pub fn six_lock_counts(lock: &c::six_lock) -> c::six_lock_count {
    let mut ret = six_lock_count::default();

    ret.n[SixLockType::SIX_LOCK_read as usize] = if !lock.readers.allocated() {
        atomic_read(lock) & SIX_LOCK_HELD_read
    } else {
        pcpu_read_count(lock)
    };
    ret.n[SixLockType::SIX_LOCK_intent as usize] = (atomic_read(lock) & SIX_LOCK_HELD_intent != 0) as c_uint +
        lock.intent_lock_recurse.get();
    ret.n[SixLockType::SIX_LOCK_write as usize] = (atomic_read(lock) & SIX_LOCK_HELD_write != 0) as c_uint;

    ret
}

/*
 * six_lock_readers_add - directly manipulate reader count of a lock
 * @lock:	lock to add/subtract readers for
 * @nr:		reader count to add/subtract
 *
 * When an upper layer is implementing lock reentrency, we may have both read
 * and intent locks on the same lock.
 *
 * When we need to take a write lock, the read locks will cause self-deadlock,
 * because six locks themselves do not track which read locks are held by the
 * current thread and which are held by a different thread - it does no
 * per-thread tracking of held locks.
 *
 * The upper layer that is tracking held locks may however, if trylock() has
 * failed, count up its own read locks, subtract them, take the write lock, and
 * then re-add them.
 *
 * As in any other situation when taking a write lock, @lock must be held for
 * intent one (or more) times, so @lock will never be left unlocked.
 */
/// # Safety
/// @lock is held for intent by the caller, and @nr takes away only read locks
/// it holds, or gives back those it took.
pub unsafe fn six_lock_readers_add(lock: &c::six_lock, nr: c_int) {
    if lock.readers.allocated() {
        lock.readers.get().add(nr as u32);
    } else {
        ebug_assert!((atomic_read(lock) & SIX_LOCK_HELD_read) as c_int + nr >= 0);
        /* reader count starts at bit 0 */
        lock.state.fetch_add(nr, Relaxed);
    }
}

unsafe impl RawLockBase for c::six_lock {
    type GuardState = ();
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), Infallible> {
        unsafe { __six_lock_init(l, name.as_ptr().cast(), key, SixLockInitFlags::empty(), 0) };
        Ok(())
    }

    unsafe fn destroy(&self) {
        unsafe { six_lock_exit(core::ptr::from_ref(self).cast_mut()) }
    }
}

impl<G, P> Locked<c::six_lock, G, P> {
    /// The lock's state, for what reads it - its seq, its counts - and for
    /// six's functions, while btree locking takes node locks through them
    /// rather than a guard.
    pub fn six(&self) -> &c::six_lock {
        self.raw()
    }
}

pub type SixReadGuard<'a, G = (), P = ()> = SharedGuard<'a, c::six_lock, G, P, Shared>;
pub type SixIntentGuard<'a, G = (), P = ()> = SharedGuard<'a, c::six_lock, G, P, SixIntent>;
/// Write, over intent: both the guard's.
pub type SixWriteGuard<'a, G = (), P = ()> = ExclusiveGuard<'a, c::six_lock, G, P, SixWriteIntent>;

impl<G: Sync, P> Locked<c::six_lock, G, P> {
    /// As @ty, read or intent: write is had over intent,
    /// SixHold::write_waiting_over().
    pub fn try_lock_as(&self, ty: SixLockType) -> Option<SixHold<'_, G, P>> {
        assert!(ty != SixLockType::SIX_LOCK_write, "a six write lock is taken over intent");
        six_trylock_ip(self.raw(), ty, 0).then(|| unsafe { SixHold::from_raw(self, ty) })
    }

    /// As @ty, if the lock's seq is still @seq: nothing's written it since.
    pub fn relock_as(&self, ty: SixLockType, seq: c_uint) -> Option<SixHold<'_, G, P>> {
        assert!(ty != SixLockType::SIX_LOCK_write, "a six write lock is taken over intent");
        six_relock_ip(self.raw(), ty, seq, 0).then(|| unsafe { SixHold::from_raw(self, ty) })
    }

    /// six_lock_ip_waiter() - six_lock_contended(), for a caller whose trylock
    /// just failed: @wait on the lock's waitlist while it waits, where other
    /// threads' cycle detection reads it; @should_sleep_fn's error if it gives
    /// up.
    pub fn lock_waiting_as(&self, ty: SixLockType, wait: &c::six_lock_waiter,
                           should_sleep_fn: impl FnMut() -> c_int, ip: c_ulong,
                           contended: bool) -> Result<SixHold<'_, G, P>, c_int> {
        assert!(ty != SixLockType::SIX_LOCK_write, "a six write lock is taken over intent");
        match six_lock_waiting(self.raw(), ty, wait, should_sleep_fn, ip, contended) {
            0 => Ok(unsafe { SixHold::from_raw(self, ty) }),
            e => Err(e),
        }
    }
}

fn six_lock_waiting(lock: &c::six_lock, ty: SixLockType, wait: &c::six_lock_waiter,
                    should_sleep_fn: impl FnMut() -> c_int, ip: c_ulong, contended: bool) -> c_int {
    if contended {
        six_lock_contended(lock, ty, wait, should_sleep_fn, ip)
    } else {
        six_lock_ip_waiter(lock, ty, wait, should_sleep_fn, ip)
    }
}

/// A hold on a six lock, its mode had at runtime - as a btree path records
/// its locks.
pub enum SixHold<'a, G = (), P = ()> {
    Read(SixReadGuard<'a, G, P>),
    Intent(SixIntentGuard<'a, G, P>),
    Write(SixWriteGuard<'a, G, P>),
}

impl<'a, G: Sync, P> SixHold<'a, G, P> {
    /// @lock, held as @ty: what into_raw() gave up, taken back.
    ///
    /// # Safety
    /// @lock is held as @ty by the caller, its to give the guard - or, for a
    /// guard never dropped, for as long as the guard lives.
    pub unsafe fn from_raw(lock: &'a Locked<c::six_lock, G, P>, ty: SixLockType) -> Self {
        unsafe {
            match ty {
                SixLockType::SIX_LOCK_read   => SixHold::Read(SharedGuard::from_held(lock, ())),
                SixLockType::SIX_LOCK_intent => SixHold::Intent(SharedGuard::from_held(lock, ())),
                SixLockType::SIX_LOCK_write  => SixHold::Write(ExclusiveGuard::from_held(lock, Some(()))),
            }
        }
    }

    /// The hold, given up to what records it outside a guard - a btree path's
    /// nodes_locked, or C: what it's held as.
    pub fn into_raw(self) -> SixLockType {
        let ty = self.ty();
        core::mem::forget(self);
        ty
    }

    pub fn ty(&self) -> SixLockType {
        match self {
            SixHold::Read(_)   => SixLockType::SIX_LOCK_read,
            SixHold::Intent(_) => SixLockType::SIX_LOCK_intent,
            SixHold::Write(_)  => SixLockType::SIX_LOCK_write,
        }
    }

    pub fn lock(&self) -> &'a Locked<c::six_lock, G, P> {
        match self {
            SixHold::Read(g)   => g.lock(),
            SixHold::Intent(g) => g.lock(),
            SixHold::Write(g)  => g.lock(),
        }
    }

    /// Another hold, as @ty, no stronger than this one: six_lock_increment().
    pub fn increment(&self, ty: SixLockType) -> Self {
        assert!(ty != SixLockType::SIX_LOCK_write && ty as u32 <= self.ty() as u32,
                "a six lock held as {} incremented as {}", self.ty() as u32, ty as u32);
        unsafe {
            six_lock_increment_ip(self.lock().raw(), ty, 0);
            SixHold::from_raw(self.lock(), ty)
        }
    }

    /// Intent to read, write to intent: another hold as the weaker, then this
    /// one dropped.
    pub fn downgrade(self) -> Self {
        let weaker = match self.ty() {
            SixLockType::SIX_LOCK_write  => SixLockType::SIX_LOCK_intent,
            SixLockType::SIX_LOCK_intent => SixLockType::SIX_LOCK_read,
            SixLockType::SIX_LOCK_read   => panic!("a six read lock downgraded"),
        };
        let down = self.increment(weaker);
        drop(self);
        down
    }

    /// Write over this intent, if no one holds the lock read: the one hold,
    /// write and intent, released together. Intent back if not.
    pub fn try_write(self) -> Result<Self, Self> {
        assert!(self.ty() == SixLockType::SIX_LOCK_intent, "a six write lock tried over a hold that isn't intent");
        let locked = self.lock();
        if !six_trylock_ip(locked.raw(), SixLockType::SIX_LOCK_write, 0) {
            return Err(self);
        }
        self.into_raw();
        Ok(unsafe { SixHold::from_raw(locked, SixLockType::SIX_LOCK_write) })
    }

    /// Read to intent, if no one else holds intent: six_lock_tryupgrade().
    pub fn try_upgrade(self) -> Result<Self, Self> {
        if self.ty() != SixLockType::SIX_LOCK_read {
            return Err(self);
        }
        let locked = self.lock();
        let lock = locked.raw();
        let mut old = atomic_read(lock);

        loop {
            let mut new = old;

            if new & SIX_LOCK_HELD_intent != 0 {
                return Err(self);
            }

            if !lock.readers.allocated() {
                ebug_assert!(new & SIX_LOCK_HELD_read != 0);
                new = new.wrapping_sub(L[SixLockType::SIX_LOCK_read as usize].lock_val);
            }

            new |= SIX_LOCK_HELD_intent;

            match lock.state.compare_exchange_weak(old as i32, new as i32, Acquire, Relaxed) {
                Ok(_)    => break,
                Err(cur) => old = cur as u32,
            }
        }

        if lock.readers.allocated() {
            lock.readers.get().add(1u32.wrapping_neg());
        }

        six_set_owner(lock, SixLockType::SIX_LOCK_intent, old, sched::current());

        self.into_raw();
        Ok(unsafe { SixHold::from_raw(locked, SixLockType::SIX_LOCK_intent) })
    }

    /// Write over this intent, as lock_waiting_as() waits, with @readers read
    /// holds set aside meanwhile: write waits for every reader to go, the
    /// intent holder's own too. On failure, intent still held.
    ///
    /// # Safety
    /// @readers are read holds the caller has on the lock.
    pub unsafe fn write_waiting_over(self, readers: c_int, wait: &c::six_lock_waiter,
                                     should_sleep_fn: impl FnMut() -> c_int,
                                     ip: c_ulong) -> Result<Self, (Self, c_int)> {
        assert!(self.ty() == SixLockType::SIX_LOCK_intent, "a six write lock taken over a hold that isn't intent");
        let locked = self.lock();
        let lock = locked.raw();

        if readers != 0 {
            unsafe { six_lock_readers_add(lock, -readers) };
        }
        let ret = six_lock_waiting(lock, SixLockType::SIX_LOCK_write, wait, should_sleep_fn, ip, readers == 0);
        if readers != 0 {
            unsafe { six_lock_readers_add(lock, readers) };
        }

        if ret != 0 {
            return Err((self, ret));
        }
        self.into_raw();
        Ok(unsafe { SixHold::from_raw(locked, SixLockType::SIX_LOCK_write) })
    }

}

impl c::six_lock {
    /// six_lock_ip(), without a should_sleep_fn: it can't fail.
    fn lock_type(&self, ty: SixLockType) {
        let wait = c::six_lock_waiter::default();
        let ret = six_lock_ip_waiter(self, ty, &wait, || 0, 0);
        ebug_assert!(ret == 0, "six lock without should_sleep_fn failed: {ret}");
    }
}

unsafe impl RawReadLock for c::six_lock {
    unsafe fn lock_read(&self) {
        self.lock_type(SixLockType::SIX_LOCK_read)
    }

    unsafe fn unlock_read(&self, _: ()) {
        unsafe { six_unlock_ip(self, SixLockType::SIX_LOCK_read, 0) }
    }
}

unsafe impl RawSixLock for c::six_lock {
    unsafe fn lock_intent(&self) {
        self.lock_type(SixLockType::SIX_LOCK_intent)
    }

    unsafe fn unlock_intent(&self, _: ()) {
        unsafe { six_unlock_ip(self, SixLockType::SIX_LOCK_intent, 0) }
    }

    unsafe fn lock_write(&self) {
        self.lock_type(SixLockType::SIX_LOCK_write)
    }

    unsafe fn unlock_write(&self, _: ()) {
        unsafe { six_unlock_ip(self, SixLockType::SIX_LOCK_write, 0) }
    }
}

/*
 * six_lock_exit - release resources held by a lock prior to freeing
 * @lock:	lock to exit
 *
 * When a lock was initialized in percpu mode (SIX_OLCK_INIT_PCPU), this is
 * required to free the percpu read counts.
 */
/// # Safety
/// Nothing holds @lock, or will.
pub unsafe fn six_lock_exit(lock: *mut c::six_lock) {
    unsafe {
        crate::warn_on!((*lock).readers.allocated() && pcpu_read_count(&*lock) != 0);
        crate::warn_on!(atomic_read(&*lock) & SIX_LOCK_HELD_read != 0);

        (*lock).readers.free();

        ManuallyDrop::drop(&mut (*(&(*lock).wait_lock).data_ptr().cast_mut()).list);
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        drop(core::mem::take(&mut *(*lock).owner_stack.get()));
    }
}

/// # Safety
/// Nothing holds or reaches @lock until this returns. What init doesn't set
/// keeps what it was - zeroes, or six_lock_exit()'s: seq, and readers but
/// with SIX_LOCK_INIT_PCPU.
pub unsafe fn __six_lock_init(lock: *mut c::six_lock, name: *const c_char, key: *mut c_void,
                              flags: c::SixLockInitFlags, gfp: c::gfp_t) {
    let wait_lock = crate::new_lock!(six_lock_waiters { next_free_hint: 0, list: ManuallyDrop::new(WaitList::new_zeroed()) },
                                     "&lock->wait_lock");
    unsafe {
        (*lock).state.store(0, Relaxed);
        let Ok(()) = init_at(wait_lock, &raw mut (*lock).wait_lock);
        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        rust_debug_check_no_locks_freed(lock.cast(), size_of::<six_lock>() as c_ulong);
        LockdepMap::init(&raw mut (*lock).dep_map, name, key);

        /*
         * Don't assume that we have real percpu variables available in
         * userspace:
         */
        #[cfg(kernel)]
        if flags.contains(SixLockInitFlags::SIX_LOCK_INIT_PCPU) {
            /*
             * We don't return an error here on memory allocation failure
             * since percpu is an optimization, and locks will work with the
             * same semantics in non-percpu mode: callers can check for
             * failure if they wish by checking lock->readers, but generally
             * will not want to treat it as an error.
             */
            (*lock).readers = PerCpu::alloc_gfp(gfp).unwrap_or_else(|_| pin_init::Zeroable::zeroed());
        }
        #[cfg(not(kernel))]
        let _ = (flags, gfp);
    }
}
