// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types, non_upper_case_globals)]

#[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
use super::raw::rust_debug_check_no_locks_freed;
use super::raw::{rust_trace_contention_begin, rust_trace_contention_end};
use super::{Guard, Locked, LockdepMap, RawLock};
use crate::cstructs::c;
#[cfg(CONFIG_SMP)]
use crate::debug::params::six_optimistic_spin_disabled;
use crate::types::c_default;
use crate::util::ffi::c_char;
use crate::util::kernel::time_after64;
use crate::util::percpu::{CpuGuard, PerCpu};
use crate::util::projections::Volatile;
#[cfg(CONFIG_SMP)]
use crate::util::rcu;
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::ptr::null_mut;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use core::sync::atomic::{fence, AtomicBool, AtomicI32, AtomicPtr};
use cstruct_macros::{c_const, c_enum, c_export, c_typedef, CStruct};

c_enum! {
    #[closed]
    pub enum six_lock_type: u32 {
        SIX_LOCK_read,
        SIX_LOCK_intent,
        SIX_LOCK_write,
    }
}

#[repr(C)]
#[derive(CStruct)]
pub struct six_lock_waiter {
    pub trans_start_time: u64,
    pub task: *mut c::task_struct,
    pub lock_want: c::six_lock_type,
    /* the waker's handoff: set, the waiter may return and reuse this */
    #[c("bool lock_acquired")]
    pub lock_acquired: AtomicBool,
    /* Index in wait_fifo->data[], set on insert, used for O(1) self-remove. */
    pub slot_idx: u16,
}
c_default!(six_lock_waiter);

/* filled under wait_lock, read without it by the cycle detector: @w last */
#[repr(C)]
#[derive(CStruct)]
pub struct six_lock_wait_slot {
    #[c("struct six_lock_waiter *w")]
    pub w: AtomicPtr<c::six_lock_waiter>,
    pub start_time: Volatile<u64>,
}
c_default!(six_lock_wait_slot);

/*
 * RCU-swappable wait list. Not a FIFO — entries sit at fixed indices from
 * insertion to removal, so RCU readers (the cycle detector) never observe an
 * entry moving. Removal writes NULL atomically; insertion finds a free slot
 * (starting from @next_free_hint) and fills it. @nr is the high-water-mark
 * index in use and shrinks when trailing slots go tombstone.
 *
 * Grown by allocating a new struct, copying fields + entries, then
 * rcu_assign_pointer'ing the lock's wait_fifo to the new object. Old
 * heap allocations are kvfree_rcu'd so future lockless readers can
 * outrun the free.
 */
#[repr(C)]
#[derive(CStruct)]
pub struct six_lock_wait_fifo {
    pub size: u16,
    pub nr: Volatile<u16>,
    pub next_free_hint: Volatile<u16>,
    pub data: [c::six_lock_wait_slot; 0],
}
c_default!(six_lock_wait_fifo);

c_const! {
    #[c_int]
    pub const SIX_LOCK_INLINE_WAITERS: u32 = 8;
}

#[repr(C)]
#[derive(CStruct)]
pub struct six_lock {
    pub state: AtomicI32,
    /* written by the write lock's holder, read without the lock */
    pub seq: Volatile<u32>,
    #[c("unsigned __percpu *readers")]
    pub readers: PerCpu<c_uint>,
    pub intent_lock_recurse: Volatile<c_uint>,
    pub write_lock_recurse: Volatile<c_uint>,
    #[c("struct task_struct *owner")]
    pub owner: AtomicPtr<c::task_struct>,
    /* the intent lock's holder's */
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    #[c("bch_stacktrace owner_stack")]
    pub owner_stack: crate::util::ffi::Opaque<c::bch_stacktrace>,
    pub wait_lock: c::raw_spinlock_t,

    #[c("struct six_lock_wait_fifo __rcu *wait_fifo")]
    pub wait_fifo: AtomicPtr<c::six_lock_wait_fifo>,

    pub inline_fifo: c::six_lock_wait_fifo,
    pub inline_fifo_data: [c::six_lock_wait_slot; c::SIX_LOCK_INLINE_WAITERS as usize],
    #[c("struct lockdep_map dep_map")]
    pub dep_map: LockdepMap,
}
c_default!(six_lock);

c_typedef! {
    pub type six_lock_should_sleep_fn = Option<unsafe extern "C" fn(lock: *mut c::six_lock,
                                                                    *mut c::six_lock_waiter) -> c_int>;
}

c_enum! {
    #[flags]
    pub enum six_lock_init_flags: u32 {
        SIX_LOCK_INIT_PCPU = 1 << 0,
    }
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
c_const! {
    #[c_int]
    pub const SIX_LOCK_WANT_BITS: u32 = 2;
    #[c_int]
    pub const SIX_LOCK_WANT_MASK: u32 = (1 << SIX_LOCK_WANT_BITS) - 1;
}

const SIX_LOCK_HELD_read_OFFSET: u32 = 0;
const SIX_LOCK_HELD_read: u32 = !(!0u32 << 26);
const SIX_LOCK_HELD_intent: u32 = 1 << 26;
const SIX_LOCK_HELD_write: u32 = 1 << 27;
const SIX_LOCK_WAITING_read: u32 = 1 << (28 + SIX_LOCK_read);
const SIX_LOCK_WAITING_write: u32 = 1 << (28 + SIX_LOCK_write);
const SIX_LOCK_NOSPIN: u32 = 1 << 31;

struct six_lock_vals {
    /* Value we add to the lock in order to take the lock: */
    lock_val:      u32,

    /* If the lock has this value (used as a mask), taking the lock fails: */
    lock_fail:     u32,

    /* Mask that indicates lock is held for this type: */
    held_mask:     u32,

    /* Waitlist we wakeup when releasing the lock: */
    unlock_wakeup: six_lock_type,
}

static L: [six_lock_vals; 3] = [
    six_lock_vals {
        lock_val:      1 << SIX_LOCK_HELD_read_OFFSET,
        lock_fail:     SIX_LOCK_HELD_write,
        held_mask:     SIX_LOCK_HELD_read,
        unlock_wakeup: six_lock_type::SIX_LOCK_write,
    },
    six_lock_vals {
        lock_val:      SIX_LOCK_HELD_intent,
        lock_fail:     SIX_LOCK_HELD_intent,
        held_mask:     SIX_LOCK_HELD_intent,
        unlock_wakeup: six_lock_type::SIX_LOCK_intent,
    },
    six_lock_vals {
        lock_val:      SIX_LOCK_HELD_write,
        lock_fail:     SIX_LOCK_HELD_read,
        held_mask:     SIX_LOCK_HELD_write,
        unlock_wakeup: six_lock_type::SIX_LOCK_read,
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
fn six_set_owner(lock: &six_lock, ty: six_lock_type, old: u32, owner: *mut c::task_struct) {
    if ty != six_lock_type::SIX_LOCK_intent {
        return;
    }

    if old & SIX_LOCK_HELD_intent == 0 {
        ebug_assert!(lock.owner.load(Relaxed).is_null());
        lock.owner.store(owner, Relaxed);
    } else {
        ebug_assert!(lock.owner.load(Relaxed) == unsafe { c::bch2_current() });
    }
}

fn six_lock_record_acquire(lock: &six_lock, ty: six_lock_type) {
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    if ty == six_lock_type::SIX_LOCK_intent {
        unsafe { c::bch2_save_backtrace(lock.owner_stack.as_ptr(), c::bch2_current(), 1, c::GFP_ATOMIC) };
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
    Wakeup(six_lock_type),
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
fn __do_six_trylock(lock: &six_lock, ty: six_lock_type, task: *mut c::task_struct, try_: bool) -> Trylock {
    use six_lock_type::*;
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

/// The slots of the wait list at @wf: they follow it, in the lock -
/// inline_fifo_data - or in its allocation.
///
/// # Safety
/// @wf is the lock's wait list, live for 'a.
#[inline(always)]
unsafe fn wf_slots<'a>(wf: *mut six_lock_wait_fifo) -> &'a [six_lock_wait_slot] {
    unsafe { core::slice::from_raw_parts((&raw mut (*wf).data).cast(), (*wf).size as usize) }
}

/*
 * Trim @nr while the tail is all tombstones. Clamp hint inside [0, nr].
 * Caller must hold wait_lock.
 */
#[inline(always)]
unsafe fn six_lock_wait_fifo_shrink(wfp: *mut six_lock_wait_fifo) {
    let (wf, slots) = unsafe { (&*wfp, wf_slots(wfp)) };

    while wf.nr.get() > 0 && slots[wf.nr.get() as usize - 1].w.load(Relaxed).is_null() {
        wf.nr.set(wf.nr.get() - 1);
    }
    if wf.next_free_hint.get() > wf.nr.get() {
        wf.next_free_hint.set(wf.nr.get());
    }
}

/*
 * Null out the entry at @idx and update the free-slot hint. Caller must hold
 * wait_lock. Does not shrink @nr — that's done lazily by the caller when
 * convenient (e.g. at the end of a wakeup scan).
 */
#[inline(always)]
unsafe fn six_lock_wait_fifo_remove(wfp: *mut six_lock_wait_fifo, idx: u16) {
    let (wf, slots) = unsafe { (&*wfp, wf_slots(wfp)) };

    /*
     * WRITE_ONCE: the lockless cycle detector reads this slot via
     * smp_load_acquire(&.w); clearing to NULL just makes it skip the slot
     * (no dependent data, so no release needed), but it's a concurrently
     * read shared location, so the store must be marked.
     */
    slots[idx as usize].w.store(null_mut(), Relaxed);
    wf.next_free_hint.set(wf.next_free_hint.get().min(idx));
}

/*
 * Find a free slot (starting from @next_free_hint) and write the entry.
 * Returns true on success, false if the list is full.
 * Caller must hold wait_lock.
 */
#[inline(always)]
unsafe fn six_lock_wait_fifo_insert(lock: &six_lock, wait: *mut six_lock_waiter) -> bool {
    let wfp = lock.wait_fifo.load(Relaxed);
    let (wf, slots) = unsafe { (&*wfp, wf_slots(wfp)) };
    let free = |i: u16| slots[i as usize].w.load(Relaxed).is_null();

    let hint = wf.next_free_hint.get();
    let i = if free(hint) {
        hint
    } else if let Some(i) = (0..wf.nr.get()).find(|&i| free(i)) {
        i
    } else if wf.nr.get() < wf.size {
        wf.nr.get()
    } else {
        return false;
    };

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
    let slot = &slots[i as usize];
    unsafe {
        slot.start_time.set(((*wait).trans_start_time << SIX_LOCK_WANT_BITS) |
                            ((*wait).lock_want as u64 & SIX_LOCK_WANT_MASK as u64));
        (*wait).slot_idx = i;
    }
    wf.next_free_hint.set((i + 1) & (wf.size - 1));
    wf.nr.set(wf.nr.get().max(i + 1));
    slot.w.store(wait, Release);
    true
}

enum Realloc<'a> {
    /// Into the list allocated last time, and @wait inserted.
    Grown(Guard<'a, c::raw_spinlock_t>),
    /// wait_lock dropped for a list to grow into, in *@new_wf: try again.
    Allocated,
    NoMem,
}

#[inline(never)]
unsafe fn six_lock_wait_fifo_realloc<'a>(lock: &'a six_lock, wait: *mut six_lock_waiter,
                                         new_wf_ret: &mut *mut six_lock_wait_fifo,
                                         g: Guard<'a, c::raw_spinlock_t>) -> Realloc<'a> {
    let old_wf = lock.wait_fifo.load(Relaxed);
    let new_wf = *new_wf_ret;

    unsafe {
        if !new_wf.is_null() && (*new_wf).size as u32 == (*old_wf).size as u32 * 2 {
            core::ptr::copy_nonoverlapping((&raw const (*old_wf).data).cast::<six_lock_wait_slot>(),
                                           (&raw mut (*new_wf).data).cast::<six_lock_wait_slot>(),
                                           (*old_wf).nr.get() as usize);
            (*new_wf).nr.set((*old_wf).nr.get());
            lock.wait_fifo.store(new_wf, Release);

            *new_wf_ret = if !core::ptr::eq(old_wf, &lock.inline_fifo) { old_wf } else { null_mut() };
            let inserted = six_lock_wait_fifo_insert(lock, wait);
            ebug_assert!(inserted);
            return Realloc::Grown(g);
        }

        let old_size = (*old_wf).size;

        drop(g);

        c::rust_kvfree(new_wf.cast());
        *new_wf_ret = null_mut();

        if old_size as u32 >= 1 << 15 {
            return Realloc::NoMem;
        }

        let new_size = old_size * 2;
        let new_wf = c::rust_kvzalloc(size_of::<six_lock_wait_fifo>() +
                                      new_size as usize * size_of::<six_lock_wait_slot>())
            .cast::<six_lock_wait_fifo>();
        if new_wf.is_null() {
            return Realloc::NoMem;
        }

        (*new_wf).size = new_size;
        *new_wf_ret = new_wf;
    }
    Realloc::Allocated
}

/*
 * Wake the single matching waiter with the smallest start_time (oldest
 * transaction). On release, only that slot is nulled — the rest of the list
 * remains stable from the lockless cycle detector's perspective.
 *
 * For reads: wake all matching waiters, same start-time ordering isn't
 * relevant since readers don't conflict with each other.
 */
unsafe fn __six_lock_wakeup(lock: &six_lock, mut lock_type: six_lock_type) {
    let wfp = lock.wait_fifo.load(Relaxed);
    let (wf, slots) = unsafe { (&*wfp, wf_slots(wfp)) };
    let matches = |slot: &six_lock_wait_slot, ty: six_lock_type|
        !slot.w.load(Relaxed).is_null() && slot.start_time.get() & SIX_LOCK_WANT_MASK as u64 == ty as u64;

    /*
     * Similar to percpu_rwsem_wake_function(), we need to
     * guard against the wakee noticing w->lock_acquired,
     * returning, and then exiting before we do the wakeup:
     */
    let wake = |i: u16| unsafe {
        let w = slots[i as usize].w.load(Relaxed);
        let task = (*w).task;

        c::get_task_struct(task);
        six_lock_wait_fifo_remove(wfp, i);
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
    };

    loop {
        let ret = 'out: {
            if lock_type == six_lock_type::SIX_LOCK_read {
                /* Readers don't conflict: wake all matching waiters. */
                for i in 0..wf.nr.get() {
                    let slot = &slots[i as usize];
                    if !matches(slot, lock_type) {
                        continue;
                    }

                    let ret = __do_six_trylock(lock, lock_type, unsafe { (*slot.w.load(Relaxed)).task }, false);
                    if ret != Trylock::Acquired {
                        break 'out ret;
                    }

                    wake(i);
                }
            } else {
                /*
                 * Intent/write locks have a single holder: prefer the waiter
                 * whose transaction started earliest. start_time is cached in
                 * the slot so the scan doesn't deref each waiter.
                 */
                let mut oldest: Option<(u16, u64)> = None;
                let mut n_matches = 0;

                for i in 0..wf.nr.get() {
                    let slot = &slots[i as usize];
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
                    let w = slots[i as usize].w.load(Relaxed);
                    let ret = __do_six_trylock(lock, lock_type, unsafe { (*w).task }, false);
                    if ret != Trylock::Acquired {
                        break 'out ret;
                    }

                    wake(i);

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

    unsafe { six_lock_wait_fifo_shrink(wfp) };
}

#[inline(always)]
fn six_lock_wakeup(lock: &six_lock, state: u32, lock_type: six_lock_type) {
    if lock_type == six_lock_type::SIX_LOCK_write && state & SIX_LOCK_HELD_read != 0 {
        return;
    }

    if state & (SIX_LOCK_WAITING_read << lock_type as u32) == 0 {
        return;
    }

    let _g = Locked::from_c(&lock.wait_lock).lock();
    unsafe { __six_lock_wakeup(lock, lock_type) };
}

#[inline(always)]
fn do_six_trylock(lock: &six_lock, ty: six_lock_type, try_: bool) -> bool {
    let ret = __do_six_trylock(lock, ty, unsafe { c::bch2_current() }, try_);

    if let Trylock::Wakeup(wakeup) = ret {
        let _g = Locked::from_c(&lock.wait_lock).lock();
        unsafe { __six_lock_wakeup(lock, wakeup) };
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
#[c_export]
pub fn six_trylock_ip(lock: &c::six_lock, ty: c::six_lock_type, ip: c_ulong) -> bool {
    if !do_six_trylock(lock, ty, true) {
        return false;
    }

    if ty != six_lock_type::SIX_LOCK_write {
        lock.dep_map.acquire(true, ty == six_lock_type::SIX_LOCK_read, ip);
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
#[c_export]
pub fn six_relock_ip(lock: &c::six_lock, ty: c::six_lock_type, seq: c_uint, ip: c_ulong) -> bool {
    if lock.seq.get() != seq || !six_trylock_ip(lock, ty, ip) {
        return false;
    }

    if lock.seq.get() != seq {
        six_unlock_ip(lock, ty, ip);
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
    let _g = rcu::read_lock();
    let owner = lock.owner.load(Relaxed);
    unsafe {
        if !owner.is_null() { c::bch2_owner_on_cpu(owner) } else { !c::bch2_rt_or_dl_task_current() }
    }
}

#[cfg(CONFIG_SMP)]
unsafe fn six_optimistic_spin(lock: &six_lock, wait: *mut six_lock_waiter, ty: six_lock_type) -> bool {
    if ty == six_lock_type::SIX_LOCK_write || six_optimistic_spin_disabled() {
        return false;
    }

    {
        let _g = rcu::read_lock();
        let wfp = lock.wait_fifo.load(Acquire);
        /*
         * Only spin if we're the sole entry in the wait list. Anything
         * more and someone older might get woken ahead of us.
         */
        if unsafe { (*wfp).nr.get() != 1 || wf_slots(wfp)[0].w.load(Relaxed) != wait } {
            return false;
        }
    }

    if atomic_read(lock) & SIX_LOCK_NOSPIN != 0 {
        return false;
    }

    let _g = CpuGuard::new();
    let end_time = unsafe { c::bch2_sched_clock() } + 10 * 1000;
    let mut loop_: u32 = 0;

    while !unsafe { c::bch2_need_resched() } && six_owner_running(lock) {
        /*
         * Ensures that writes to the waitlist entry happen after we see
         * wait->lock_acquired: pairs with the smp_store_release in
         * __six_lock_wakeup
         */
        if unsafe { (*wait).lock_acquired.load(Acquire) } {
            return true;
        }

        loop_ = loop_.wrapping_add(1);
        if loop_ & 0xf == 0 && time_after64(unsafe { c::bch2_sched_clock() }, end_time) {
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
unsafe fn six_optimistic_spin(_lock: &six_lock, _wait: *mut six_lock_waiter, _ty: six_lock_type) -> bool {
    false
}

#[inline(always)]
unsafe fn __six_lock_slowpath(lock: &six_lock, ty: six_lock_type, wait: *mut six_lock_waiter,
                              should_sleep_fn: c::six_lock_should_sleep_fn, ip: c_ulong) -> c_int {
    use six_lock_type::*;
    let mut new_wf: *mut six_lock_wait_fifo = null_mut();
    let mut ret: c_int = 0;

    if ty == SIX_LOCK_write {
        ebug_assert!(atomic_read(lock) & SIX_LOCK_HELD_write == 0);
        lock.state.fetch_add(SIX_LOCK_HELD_write as i32, Relaxed);
        /* smp_mb__after_atomic() */
        fence(SeqCst);
    }

    unsafe { rust_trace_contention_begin(core::ptr::from_ref(lock).cast(), 0) };
    lock.dep_map.contended(ip);

    unsafe {
        (*wait).task = c::bch2_current();
        (*wait).lock_want = ty;
        (*wait).lock_acquired.store(false, Relaxed);
    }

    'out: {
        let acquired = loop {
            let g = Locked::from_c(&lock.wait_lock).lock();

            /*
             * Retry taking the lock after taking waitlist lock, in case we raced
             * with an unlock:
             */
            six_set_bitmask(lock, SIX_LOCK_WAITING_read << ty as u32);
            match __do_six_trylock(lock, ty, unsafe { c::bch2_current() }, false) {
                Trylock::Acquired       => break true,
                Trylock::Failed         => {}
                Trylock::Wakeup(wakeup) => unsafe { __six_lock_wakeup(lock, wakeup) },
            }

            if unsafe { six_lock_wait_fifo_insert(lock, wait) } {
                break false;
            }
            match unsafe { six_lock_wait_fifo_realloc(lock, wait, &mut new_wf, g) } {
                Realloc::Grown(g)  => {
                    drop(g);
                    break false;
                }
                Realloc::Allocated => continue,
                Realloc::NoMem     => {
                    if ty == SIX_LOCK_write {
                        six_clear_bitmask(lock, SIX_LOCK_HELD_write);
                        six_lock_wakeup(lock, atomic_read(lock), SIX_LOCK_read);
                    }
                    ret = -(c::ENOMEM as c_int);
                    break 'out;
                }
            }
        };

        if acquired {
            break 'out;
        }

        if unsafe { six_optimistic_spin(lock, wait, ty) || (*wait).lock_acquired.load(Acquire) } {
            break 'out;
        }

        /* Yield before running the cycle detector: */
        unsafe { c::schedule() };

        loop {
            unsafe { c::bch2_set_current_state(c::TASK_UNINTERRUPTIBLE) };

            /*
             * Ensures that writes to the waitlist entry happen after we see
             * wait->lock_acquired: pairs with the smp_store_release in
             * __six_lock_wakeup
             */
            if unsafe { (*wait).lock_acquired.load(Acquire) } {
                break;
            }

            ret = match should_sleep_fn {
                Some(f) => unsafe { f(core::ptr::from_ref(lock).cast_mut(), wait) },
                None    => 0,
            };
            if ret != 0 {
                /*
                 * If should_sleep_fn() returns an error, we are
                 * required to return that error even if we already
                 * acquired the lock - should_sleep_fn() might have
                 * modified external state (e.g. when the deadlock cycle
                 * detector in bcachefs issued a transaction restart)
                 */
                let acquired = {
                    let _g = Locked::from_c(&lock.wait_lock).lock();
                    let acquired = unsafe { (*wait).lock_acquired.load(Relaxed) };
                    if !acquired {
                        let wf = lock.wait_fifo.load(Relaxed);

                        unsafe {
                            six_lock_wait_fifo_remove(wf, (*wait).slot_idx);
                            six_lock_wait_fifo_shrink(wf);
                        }
                    }
                    acquired
                };

                if acquired {
                    do_six_unlock_type(lock, ty);
                } else if ty == SIX_LOCK_write {
                    six_clear_bitmask(lock, SIX_LOCK_HELD_write);
                    six_lock_wakeup(lock, atomic_read(lock), SIX_LOCK_read);
                }
                break;
            }

            unsafe { c::schedule() };
        }

        unsafe { c::bch2_set_current_state_running() };
    }

    if !new_wf.is_null() {
        unsafe { c::rust_kfree_rcu_mightsleep(new_wf.cast()) };
    }
    unsafe { rust_trace_contention_end(core::ptr::from_ref(lock).cast(), 0) };

    ret
}

#[inline(never)]
unsafe fn six_lock_slowpath(lock: &six_lock, ty: six_lock_type, wait: *mut six_lock_waiter,
                            should_sleep_fn: c::six_lock_should_sleep_fn, ip: c_ulong) -> c_int {
    unsafe { __six_lock_slowpath(lock, ty, wait, should_sleep_fn, ip) }
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
/// # Safety
/// @wait lives, untouched but by six, until this returns.
#[c_export]
pub unsafe fn six_lock_ip_waiter(lock: &c::six_lock, ty: c::six_lock_type, wait: *mut c::six_lock_waiter,
                                 should_sleep_fn: c::six_lock_should_sleep_fn, ip: c_ulong) -> c_int {
    if ty != six_lock_type::SIX_LOCK_write {
        lock.dep_map.acquire(false, ty == six_lock_type::SIX_LOCK_read, ip);
    }

    let ret = if do_six_trylock(lock, ty, true) {
        0
    } else {
        unsafe { six_lock_slowpath(lock, ty, wait, should_sleep_fn, ip) }
    };

    if ret != 0 && ty != six_lock_type::SIX_LOCK_write {
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
/// # Safety
/// As six_lock_ip_waiter()'s.
#[c_export]
pub unsafe fn six_lock_contended(lock: &c::six_lock, ty: c::six_lock_type, wait: *mut c::six_lock_waiter,
                                 should_sleep_fn: c::six_lock_should_sleep_fn, ip: c_ulong) -> c_int {
    if ty != six_lock_type::SIX_LOCK_write {
        lock.dep_map.acquire(false, ty == six_lock_type::SIX_LOCK_read, ip);
    }

    let ret = unsafe { __six_lock_slowpath(lock, ty, wait, should_sleep_fn, ip) };

    if ret != 0 && ty != six_lock_type::SIX_LOCK_write {
        lock.dep_map.release(ip);
    }
    if ret == 0 {
        lock.dep_map.acquired(ip);
        six_lock_record_acquire(lock, ty);
    }

    ret
}

#[inline(always)]
fn do_six_unlock_type(lock: &six_lock, ty: six_lock_type) {
    let l = &L[ty as usize];
    let state;

    if ty == six_lock_type::SIX_LOCK_intent {
        lock.owner.store(null_mut(), Relaxed);
    }

    if ty == six_lock_type::SIX_LOCK_read && lock.readers.allocated() {
        fence(SeqCst); /* unlock barrier */
        lock.readers.get().add(1u32.wrapping_neg());
        fence(SeqCst); /* between unlocking and checking for waiters */
        state = atomic_read(lock);
    } else {
        let mut v = l.lock_val;

        if ty != six_lock_type::SIX_LOCK_read {
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
#[c_export]
pub fn six_unlock_ip(lock: &c::six_lock, ty: c::six_lock_type, ip: c_ulong) {
    use six_lock_type::*;

    ebug_assert!(!(ty == SIX_LOCK_write && atomic_read(lock) & SIX_LOCK_HELD_intent == 0));
    ebug_assert!(!((ty == SIX_LOCK_write || ty == SIX_LOCK_intent) &&
                   lock.owner.load(Relaxed) != unsafe { c::bch2_current() }));

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
#[c_export]
pub fn six_lock_downgrade(lock: &c::six_lock) {
    let ip = six_lock_downgrade as *const () as c_ulong;

    six_lock_increment_ip(lock, six_lock_type::SIX_LOCK_read, ip);
    six_unlock_ip(lock, six_lock_type::SIX_LOCK_intent, ip);
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
#[c_export]
pub fn six_lock_tryupgrade(lock: &c::six_lock) -> bool {
    let mut old = atomic_read(lock);

    loop {
        let mut new = old;

        if new & SIX_LOCK_HELD_intent != 0 {
            return false;
        }

        if !lock.readers.allocated() {
            ebug_assert!(new & SIX_LOCK_HELD_read != 0);
            new = new.wrapping_sub(L[SIX_LOCK_read as usize].lock_val);
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

    six_set_owner(lock, six_lock_type::SIX_LOCK_intent, old, unsafe { c::bch2_current() });

    true
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
#[c_export]
pub fn six_trylock_convert(lock: &c::six_lock, from: c::six_lock_type, to: c::six_lock_type) -> bool {
    ebug_assert!(to != six_lock_type::SIX_LOCK_write && from != six_lock_type::SIX_LOCK_write);

    if to == from {
        return true;
    }

    if to == six_lock_type::SIX_LOCK_read {
        six_lock_downgrade(lock);
        true
    } else {
        six_lock_tryupgrade(lock)
    }
}

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
#[c_export]
pub fn six_lock_increment_ip(lock: &c::six_lock, ty: c::six_lock_type, ip: c_ulong) {
    use six_lock_type::*;

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
#[c_export]
pub fn six_lock_wakeup_all(lock: &c::six_lock) {
    let state = atomic_read(lock);

    /*
     * First, run the normal wakeup machinery for any waiters the lock
     * state actually permits to acquire: those slots get removed by
     * __six_lock_wakeup and lock_acquired is set on the waiter.
     */
    six_lock_wakeup(lock, state, six_lock_type::SIX_LOCK_read);
    six_lock_wakeup(lock, state, six_lock_type::SIX_LOCK_intent);
    six_lock_wakeup(lock, state, six_lock_type::SIX_LOCK_write);

    /*
     * Then wake any remaining waiters without removing their slots:
     * lock_acquired stays clear, so they'll re-run should_sleep_fn in
     * six_lock_slowpath() and self-remove via wait->slot_idx if it
     * returns an error. Removing here would corrupt that self-remove.
     */
    let _g = Locked::from_c(&lock.wait_lock).lock();
    let wfp = lock.wait_fifo.load(Relaxed);
    let (wf, slots) = unsafe { (&*wfp, wf_slots(wfp)) };
    for slot in &slots[..wf.nr.get() as usize] {
        let w = slot.w.load(Relaxed);
        if !w.is_null() {
            unsafe { c::wake_up_process((*w).task) };
        }
    }
}

/*
 * six_lock_counts - return held lock counts, for each lock type
 * @lock:	lock to return counters for
 *
 * Return: the number of times a lock is held for read, intent and write.
 */
#[c_export]
pub fn six_lock_counts(lock: &c::six_lock) -> c::six_lock_count {
    let mut ret = six_lock_count::default();

    ret.n[SIX_LOCK_read as usize] = if !lock.readers.allocated() {
        atomic_read(lock) & SIX_LOCK_HELD_read
    } else {
        pcpu_read_count(lock)
    };
    ret.n[SIX_LOCK_intent as usize] = (atomic_read(lock) & SIX_LOCK_HELD_intent != 0) as c_uint +
        lock.intent_lock_recurse.get();
    ret.n[SIX_LOCK_write as usize] = (atomic_read(lock) & SIX_LOCK_HELD_write != 0) as c_uint;

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
#[c_export]
pub fn six_lock_readers_add(lock: &c::six_lock, nr: c_int) {
    if lock.readers.allocated() {
        lock.readers.get().add(nr as u32);
    } else {
        ebug_assert!((atomic_read(lock) & SIX_LOCK_HELD_read) as c_int + nr >= 0);
        /* reader count starts at bit 0 */
        lock.state.fetch_add(nr, Relaxed);
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
#[c_export]
pub unsafe fn six_lock_exit(lock: *mut c::six_lock) {
    unsafe {
        crate::warn_on!((*lock).readers.allocated() && pcpu_read_count(&*lock) != 0);
        crate::warn_on!(atomic_read(&*lock) & SIX_LOCK_HELD_read != 0);

        (*lock).readers.free();

        let wf = (*lock).wait_fifo.load(Relaxed);
        if !core::ptr::eq(wf, &raw const (*lock).inline_fifo) {
            c::rust_kvfree(wf.cast());
        }
        (*lock).wait_fifo.store(null_mut(), Relaxed);
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        crate::util::alloc::darray_exit(&mut *(*lock).owner_stack.as_ptr());
    }
}

/// # Safety
/// Nothing holds or reaches @lock until this returns. What init doesn't set
/// keeps what it was - zeroes, or six_lock_exit()'s: seq, and readers but
/// with SIX_LOCK_INIT_PCPU.
#[c_export]
pub unsafe fn __six_lock_init(lock: *mut c::six_lock, name: *const c_char, key: *mut c_void,
                              flags: c::six_lock_init_flags, gfp: c::gfp_t) {
    unsafe {
        (*lock).state.store(0, Relaxed);
        let Ok(()) = c::raw_spinlock_t::init(&raw mut (*lock).wait_lock, c"&lock->wait_lock",
                                             crate::new_lock!(@key));
        (*lock).inline_fifo.size = SIX_LOCK_INLINE_WAITERS as u16;
        (*lock).inline_fifo.nr.set(0);
        (*lock).inline_fifo.next_free_hint.set(0);
        (*lock).wait_fifo.store(&raw mut (*lock).inline_fifo, Relaxed);
        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        rust_debug_check_no_locks_freed(lock.cast(), size_of::<six_lock>() as c_ulong);
        LockdepMap::init(&raw mut (*lock).dep_map, name, key);

        /*
         * Don't assume that we have real percpu variables available in
         * userspace:
         */
        #[cfg(kernel)]
        if flags.contains(six_lock_init_flags::SIX_LOCK_INIT_PCPU) {
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
