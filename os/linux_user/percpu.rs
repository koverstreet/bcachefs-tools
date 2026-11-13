// SPDX-License-Identifier: GPL-2.0

//! Per-CPU data in the structs C allocates it in - a `T __percpu *` field -
//! typed and operated on from Rust. The kernel has no per-CPU operations for
//! Rust (as of 7.3 a posted series is x86_64 only, and userspace has none), so
//! these are ours: C's operations, with C's contract.
//!
//! Per-CPU data is of two kinds, which want different things:
//!
//! - Counters - counters.now, io_done, ca->usage, capacity.pcpu's usage and
//!   online_reserved: added to on whatever CPU the adder is on, from any
//!   context, and only read as the sum over every CPU. Each CPU's copy is a
//!   signed delta - a reservation taken on one CPU is put on another - so the
//!   adds wrap, and a sum is exact only with the adders excluded: one that
//!   races them can come out negative. Counters says how a T sums - wrapping,
//!   and clamped for racing reads - field by field for a struct, by
//!   #[derive(Fieldwise)].
//!
//! - Slots a CPU owns - capacity.pcpu's reservation caches, a cache of a
//!   freed object: changed by their own CPU, by a read-modify-write or an
//!   xchg or cmpxchg, and at most reset by another. A slot is a Volatile:
//!   every access a relaxed atomic, so another CPU's reset is a store like
//!   any other - each_cpu() reaches every CPU's - and one racing this CPU's
//!   read-modify-write may be lost, which is what a slot is for. A struct of
//!   slots is CpuShared: every part another CPU writes a Volatile, or
//!   Opaque - a local_lock_t, which its CPU's lockers change.
//!
//! The types:
//!
//! - PerCpu<T> is the field. alloc() - or C - allocates it at init, and
//!   free() frees it at exit; between, it's live for every possible CPU,
//!   which every operation takes as given, as C's do. Not Copy: nothing holds
//!   it past its struct.
//! - PerCpuRef<'a, T> borrows it, or a part of it - at(i), percpu_field!() -
//!   as every CPU's copy: sum() over all of them; read(), add(), set(),
//!   xchg() and cmpxchg() on this CPU's, each one operation.
//! - CpuCopy<'a, T> is one CPU's, from cpus(), read volatile: to sum part of
//!   a T - percpu_field!() of each - in one sweep over the CPUs, where a sum
//!   of each part would be a sweep apiece and sum() reads all of T.
//! - ThisCpu<'g, T> is this CPU's copy, while a Pinned guard keeps it this
//!   CPU: a place, as util/projections.rs's are, or add() of a delta to a
//!   struct of counters; a slot's, its Volatile. Pinned is a CpuGuard -
//!   preemption off - an IrqGuard, interrupts off, or a LocalLockGuard,
//!   this CPU's local_lock_t held.
//!
//! What's atomic is C's contract and no more: a this_cpu operation is whole
//! against this CPU - its interrupts and preemption - not against another CPU
//! writing the same copy. arm64's happen to be, x86's aren't, so nothing may
//! lean on it. ThisCpu's read-modify-write is a load and a store, as C's
//! under preempt_disable() are: an interrupt adding to the same copy between
//! them is lost, so a counter interrupts add to goes through PerCpuRef::add().
//! And it's whole against this CPU's other tasks only as far as the guard
//! keeps them out: a CpuGuard keeps them all out.
//! Another CPU's copy is only read volatile, never through a reference: it
//! may be being written.
//!
//! But a T only its own CPU ever touches - a cache, under an IrqGuard - is
//! had by reference, as_mut(), unsafely: this CPU's while the guard keeps out
//! the rest of this CPU, and each CPU's, CpuCopy's, at teardown.
//!
//! A per-CPU pointer isn't an address - here it's an offset into a thread's
//! chunk - so nothing projects through one: a field's offset is taken in a T
//! on the stack, and a CPU's copy is found by its CPU's chunk.
//!
//! A PerCpuWord - u32 or u64 - has three: read, add, cmpxchg. xchg is a
//! cmpxchg loop over them, as x86's C is.

use core::cell::Cell;
use core::ffi::{c_uint, c_ulong, c_void};
use core::marker::PhantomData;
use core::mem::{align_of, size_of, MaybeUninit};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::c;
use crate::locking::{rust_local_irq_restore, rust_local_irq_save, rust_local_lock_init, rust_local_lock_irqsave,
                     rust_local_unlock_irqrestore, MemallocFlags};
use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use crate::util::projections::{AtomicWord, FakePlaceRef, FakePlaceWrite, FakePlaceRead, Volatile};

/// Nothing moves a thread between CPUs here: each thread is its own.
pub fn migrate_disable() {}

pub fn migrate_enable() {}

/// 0, as the shim's cpumask.h had it.
pub fn raw_smp_processor_id() -> c_uint {
    0
}

// ── The per-CPU areas: a chunk per thread ───────────────────────────────

/*
 * A percpu pointer is an offset into a chunk, and resolves to that offset in
 * whichever CPU's chunk is wanted - which is also how the kernel does it, see
 * __addr_to_pcpu_ptr() in mm/percpu.c. Offset 0 is never handed out: a null
 * PerCpu is an unallocated one.
 *
 * A thread takes a slot in thread_init(), called from linux_thread_init() or
 * on its first percpu access. alloc_init()'s initializers are a registry, run
 * for every live chunk plus future ones.
 *
 * A slot is a CPU: a thread holds one while it lives and returns it when it
 * exits, and the next new thread takes it over, chunk contents and all. So
 * MAX_CPUS bounds live threads, not threads ever created - which matters for
 * kernel code that starts a short-lived kthread per operation (reconcile's
 * per-device passes), each of which used to take a slot for good.
 *
 * Chunks are never unmapped, and a published slot never changes: readers
 * walk [0, bch_percpu_nr_cpus) unlocked, as the kernel walks its possible CPUs.
 * Nothing is torn down at exit either - exit() runs destructors while other
 * threads (workqueue workers, the shrinker thread, liburcu's call_rcu thread)
 * are still running and may still touch percpu memory, and the process's
 * memory goes away with it anyway.
 *
 * The allocator is bump + freelist. Allocations return zeroed memory across
 * all live chunks; new threads get zeroed chunks via anonymous mmap, which
 * preserves the contract on subsequent allocations.
 */

pub const MAX_CPUS: usize = 256;

const GRAIN: usize = 8;

/*
 * Address space per chunk, not memory: chunks are NORESERVE anonymous
 * mappings, so pages materialize only as allocations touch them. Sized for the
 * largest consumer - gc's accounting table allocates a percpu counter set per
 * accounting key, and a large filesystem with many snapshots has hundreds of
 * thousands of those.
 *
 * On 64 bit we can be generous - but a 32 bit process has ~3G of address space
 * to share between every thread's chunk and everything else, so it gets a much
 * smaller reservation.
 */
const CHUNK_SIZE: usize = if size_of::<usize>() > 4 { 256 << 20 } else { 8 << 20 };

static CHUNKS: [AtomicPtr<u8>; MAX_CPUS] = [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];

/* include/linux/cpumask.h's possible CPUs */
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static bch_percpu_nr_cpus: AtomicI32 = AtomicI32::new(0);

thread_local! {
    static MY_CHUNK: Cell<*mut u8> = const { Cell::new(ptr::null_mut()) };
    static MY_ID:    Cell<usize>   = const { Cell::new(0) };
}

struct FreeRun {
    off:  usize,
    size: usize,
}

struct DynamicInit {
    pcv:  usize,
    init: InitCopy<c_void, c_void>,
    ctx:  *mut c_void,
}

struct State {
    /*
     * Slots whose thread has exited. A slot is a CPU, not a thread: its chunk
     * stays mapped and published, and what the old thread left in it - counter
     * contributions, freelists - carries on for whichever thread takes the
     * slot next, as a CPU's percpu state does across the tasks that run on it.
     */
    free_slots:    Vec<usize>,
    exit_key:      Option<libc::pthread_key_t>,
    free_runs:     Vec<FreeRun>,
    dynamic_inits: Vec<DynamicInit>,
    dynamic_used:  usize,
    /*
     * Map from grain index to allocation size in grains, so free_percpu()
     * doesn't need a size argument.
     *
     * Grown to cover dynamic_used rather than sized to the whole arena: the
     * bump allocator only moves forward, and free_percpu() only ever looks up
     * a grain that was allocated, so nothing above the high water mark is ever
     * read.
     */
    size_at_grain: Vec<u32>,
}

// A DynamicInit's ctx is for its init, on whichever thread runs it
unsafe impl Send for State {}

static STATE: Mutex<State> = Mutex::new(State {
    free_slots:    Vec::new(),
    exit_key:      None,
    free_runs:     Vec::new(),
    dynamic_inits: Vec::new(),
    dynamic_used:  GRAIN,
    size_at_grain: Vec::new(),
});

fn lock() -> MutexGuard<'static, State> {
    STATE.lock().expect("percpu lock poisoned")
}

fn possible_cpus() -> impl Iterator<Item = c_uint> {
    0..bch_percpu_nr_cpus.load(Ordering::Acquire) as c_uint
}

fn per_cpu_ptr<T>(pcp: *const T, cpu: c_uint) -> *mut T {
    CHUNKS[cpu as usize].load(Ordering::Relaxed).wrapping_add(pcp.addr()).cast()
}

/*
 * Lazy-init: any thread that takes a percpu pointer gets a chunk on first
 * access. Avoids having to remember to call thread_init() at the entry point
 * of every thread that might reach libbcachefs - Rust-spawned threads
 * (tiny_http workers, fuse workers, std::thread closures we don't directly
 * own) won't hit a NULL chunk and silently corrupt / SIGSEGV.
 */
#[inline(always)]
fn my_chunk() -> *mut u8 {
    let chunk = MY_CHUNK.get();
    if !chunk.is_null() {
        return chunk;
    }
    thread_init();
    MY_CHUNK.get()
}

#[inline(always)]
fn this_cpu<T>(pcp: *const T) -> *mut T {
    my_chunk().wrapping_add(pcp.addr()).cast()
}

unsafe fn this_cpu_ptr<T>(pcp: *mut T, _g: &impl Pinned) -> *mut T {
    this_cpu(pcp)
}

/*
 * The key's value is only there so that its destructor runs at thread exit:
 * pthread runs a destructor only for keys with a non-NULL value.
 */
fn claim_slot(key: libc::pthread_key_t, id: usize, chunk: *mut u8) {
    MY_CHUNK.set(chunk);
    MY_ID.set(id);
    unsafe { libc::pthread_setspecific(key, chunk.cast()) };
}

/*
 * Runs after the thread's Rust thread_local destructors (glibc runs
 * __call_tls_dtors() before pthread key destructors), so those may still use
 * percpu memory. If anything after this does, it takes a slot again and sets
 * the key again, and pthread calls us again.
 */
unsafe extern "C" fn thread_exit(_chunk: *mut c_void) {
    lock().free_slots.push(MY_ID.get());
    MY_CHUNK.set(ptr::null_mut());
}

/*
 * Running out of slots means something made more threads than expected; say
 * which. Workqueue workers are named "<queue>/<n>", so they're grouped by
 * queue.
 */
fn print_threads() {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else { return };

    let mut names = std::collections::BTreeMap::<String, u32>::new();
    for task in dir.flatten() {
        if let Ok(comm) = std::fs::read_to_string(task.path().join("comm")) {
            let name = comm.split(['/', '\n']).next().unwrap_or_default();
            *names.entry(name.to_owned()).or_default() += 1;
        }
    }

    eprintln!("{} live threads:", names.values().sum::<u32>());
    for (name, nr) in names {
        eprintln!("  {name:<16} {nr}");
    }
}

pub(crate) fn thread_init() {
    if !MY_CHUNK.get().is_null() {
        return;
    }

    let mut s = lock();

    let key = match s.exit_key {
        Some(key) => key,
        None => {
            let mut key = 0;
            let ret = unsafe { libc::pthread_key_create(&mut key, Some(thread_exit)) };
            if ret != 0 {
                eprintln!("bch_percpu_thread_init: pthread_key_create: {}",
                          std::io::Error::from_raw_os_error(ret));
                std::process::abort();
            }
            *s.exit_key.insert(key)
        }
    };

    if let Some(id) = s.free_slots.pop() {
        /* Already published and initialized - it carries on as it was: */
        claim_slot(key, id, CHUNKS[id].load(Ordering::Relaxed));
        return;
    }

    let id = bch_percpu_nr_cpus.load(Ordering::Relaxed) as usize;
    if id >= MAX_CPUS {
        eprintln!("bch_percpu_thread_init: too many live threads (max {MAX_CPUS})");
        print_threads();
        std::process::abort();
    }

    /* Address space, not memory - pages fault in as they're touched: */
    let chunk = unsafe {
        libc::mmap(ptr::null_mut(), CHUNK_SIZE, libc::PROT_READ | libc::PROT_WRITE,
                   libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0)
    };
    if chunk == libc::MAP_FAILED {
        eprintln!("bch_percpu_thread_init: chunk mmap failed: {}", std::io::Error::last_os_error());
        std::process::abort();
    }
    let chunk = chunk.cast::<u8>();

    CHUNKS[id].store(chunk, Ordering::Relaxed);
    claim_slot(key, id, chunk);

    for di in &s.dynamic_inits {
        unsafe { (di.init)(chunk.wrapping_add(di.pcv).cast(), di.ctx, id as c_uint) };
    }

    /*
     * Publish the slot last. Readers don't take the lock: they walk
     * [0, bch_percpu_nr_cpus) and per_cpu_ptr() dereferences the chunk
     * without a NULL check, so bumping the count before the chunk is
     * installed and initialized hands them a NULL - or a chunk whose
     * counters haven't been zeroed yet.
     */
    bch_percpu_nr_cpus.store(id as i32 + 1, Ordering::Release);
}

impl State {
    fn dynamic_alloc(&mut self, size: usize) -> Option<usize> {
        if let Some(i) = self.free_runs.iter().position(|run| run.size >= size) {
            let run = &mut self.free_runs[i];
            let off = run.off;
            if run.size > size {
                run.off  += size;
                run.size -= size;
            } else {
                self.free_runs.remove(i);
            }
            return Some(off);
        }

        if self.dynamic_used + size > CHUNK_SIZE {
            return None;
        }

        let off = self.dynamic_used;
        self.dynamic_used += size;
        self.size_at_grain.resize(self.dynamic_used / GRAIN, 0);
        Some(off)
    }
}

fn alloc_percpu(size: usize, align: usize) -> *mut c_void {
    /*
     * Zero has no grain to record a size in: the free list would match the
     * first run without consuming anything and hand the same offset out
     * again. No caller wants a zero sized percpu variable anyway.
     */
    assert!(size != 0);
    /* Honored implicitly: all offsets are grain-aligned */
    assert!(align <= GRAIN);
    let size = size.next_multiple_of(GRAIN);

    /* A PerCpu always has a CPU, for sum() to start from: its allocator's */
    thread_init();

    let mut s = lock();

    let Some(off) = s.dynamic_alloc(size) else {
        eprintln!("alloc_percpu: dynamic arena exhausted (used {}, requested {size}, max {CHUNK_SIZE})",
                  s.dynamic_used);
        return ptr::null_mut();
    };

    s.size_at_grain[off / GRAIN] = (size / GRAIN) as u32;

    /*
     * Zero across all live chunks (covers reuse from free list; new threads
     * get zero-filled mmap'd chunks so the slot is already zero in chunks
     * created later).
     */
    let p = ptr::without_provenance_mut::<u8>(off);
    for cpu in possible_cpus() {
        unsafe { per_cpu_ptr(p, cpu).write_bytes(0, size) };
    }
    p.cast()
}

/// A PerCpu's into_raw(), freed.
///
/// # Safety
/// Nothing uses @p after, on any CPU.
pub unsafe fn free_percpu(p: *mut c_void) {
    if p.is_null() {
        return;
    }

    let off = p.addr();
    let mut s = lock();

    /*
     * Everything else is a caller bug: a pointer from somewhere other than
     * alloc_percpu(), or a double free. Returning quietly turns any of those
     * into an arena slot that is never reused and never reported - so say so
     * instead.
     */
    let size = match s.size_at_grain.get_mut(off / GRAIN) {
        Some(size) if *size != 0 && off % GRAIN == 0 => core::mem::take(size) as usize * GRAIN,
        _ => panic!("free_percpu: {p:p} isn't allocated"),
    };

    /* Its init is its own: run on threads after, it writes freed memory */
    s.dynamic_inits.retain(|di| di.pcv != off);
    s.free_runs.push(FreeRun { off, size });
}

// ── The operations ───────────────────────────────────────────────────────

/// A word per-CPU data is operated on in: u32 or u64. Its operations are
/// this CPU's copy's; xchg is a cmpxchg loop over them, as C's is.
///
/// # Safety
/// Each is C's this_cpu_*() of a Self: for a @pcp to a per-CPU Self,
/// allocated for every possible CPU and live.
pub unsafe trait PerCpuWord: Copy + Eq {
    unsafe fn this_cpu_read(pcp: *mut Self) -> Self;
    unsafe fn this_cpu_add(pcp: *mut Self, val: Self);
    unsafe fn this_cpu_cmpxchg(pcp: *mut Self, old: Self, new: Self) -> Self;
}

macro_rules! per_cpu_word {
    ($($t:ty, $a:ty);*) => { $(
        unsafe impl PerCpuWord for $t {
            #[inline(always)]
            unsafe fn this_cpu_read(pcp: *mut $t) -> $t {
                unsafe { <$a>::from_ptr(this_cpu(pcp)) }.load(Ordering::Relaxed)
            }

            #[inline(always)]
            unsafe fn this_cpu_add(pcp: *mut $t, val: $t) {
                let v = unsafe { <$a>::from_ptr(this_cpu(pcp)) };
                v.store(v.load(Ordering::Relaxed).wrapping_add(val), Ordering::Relaxed);
            }

            #[inline(always)]
            unsafe fn this_cpu_cmpxchg(pcp: *mut $t, old: $t, new: $t) -> $t {
                let v = unsafe { <$a>::from_ptr(this_cpu(pcp)) };
                let prev = v.load(Ordering::Relaxed);
                if prev == old {
                    v.store(new, Ordering::Relaxed);
                }
                prev
            }
        }
    )* };
}
per_cpu_word!(u32, AtomicU32; u64, AtomicU64);

/// The task held to this CPU while it lives: for this CPU's copy, which
/// is the copy of whatever CPU the task is on.
///
/// # Safety
/// While a value lives the task doesn't migrate. Not Send or Sync: it's
/// this CPU's.
pub unsafe trait Pinned {}

/// Preemption off while it lives, and a NORECLAIM scope. Not Send or Sync:
/// it's this CPU's.
pub struct CpuGuard {
    _noreclaim: MemallocFlags,
    _p:         PhantomData<*const ()>,
}

// SAFETY: a thread is its own CPU here
unsafe impl Pinned for CpuGuard {}

impl CpuGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        CpuGuard { _noreclaim: MemallocFlags::noreclaim(), _p: PhantomData }
    }
}

/// This CPU's interrupts off while it lives, as local_irq_save(), and a
/// NORECLAIM scope: for a slot interrupt handlers change too - a read bio's
/// completion - which a CpuGuard's ThisCpu would race. Not Send or Sync: it's
/// this CPU's.
pub struct IrqGuard {
    flags:      c_ulong,
    _noreclaim: MemallocFlags,
    _p:         PhantomData<*const ()>,
}

// SAFETY: interrupts off, so no preemption either
unsafe impl Pinned for IrqGuard {}

impl IrqGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let flags = unsafe { rust_local_irq_save() };
        IrqGuard { flags, _noreclaim: MemallocFlags::noreclaim(), _p: PhantomData }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        // SAFETY: what new() saved, on this CPU
        unsafe { rust_local_irq_restore(self.flags) }
    }
}

/// This CPU's copy of a per-CPU local_lock_t, held: local_lock_irqsave(),
/// from PerCpuRef::lock_irqsave(). It pins the task and keeps out this CPU's
/// interrupts and other takers: interrupts off - or on PREEMPT_RT, a sleeping
/// lock with migration off, so what it guards may take sleeping locks. A
/// ThisCpu under it is whole only for what the lock guards.
pub struct LocalLockGuard<'a> {
    pcp:        *mut c::local_lock_t,
    flags:      c_ulong,
    _noreclaim: MemallocFlags,
    _p:         PhantomData<(&'a c::local_lock_t, *const ())>,
}

// SAFETY: migration's off while it's held - by the lock on RT, interrupts
// off elsewhere
unsafe impl Pinned for LocalLockGuard<'_> {}

impl Drop for LocalLockGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this CPU's, which it took
        unsafe { rust_local_unlock_irqrestore(self.pcp, self.flags) }
    }
}

// ── PerCpu, and its copies ───────────────────────────────────────────────

/// C's `T __percpu *`: see the top of the file.
#[repr(transparent)]
pub struct PerCpu<T>(*mut T);

// Null: not yet allocated, as C's init finds it.
unsafe impl<T> ::pin_init::Zeroable for PerCpu<T> {}

/// A PerCpu, or a part of one, for as long as it's borrowed.
pub struct PerCpuRef<'a, T>(*mut T, PhantomData<&'a PerCpu<T>>);

impl<T> Clone for PerCpuRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for PerCpuRef<'_, T> {}

impl<T: ::pin_init::Zeroable> PerCpu<T> {
    /// Every possible CPU's copy, zeroed: alloc_percpu().
    pub fn alloc() -> Result<Self, AllocError> {
        Self::alloc_gfp(c::GFP_KERNEL)
    }

    /// alloc(), allocating with @gfp: alloc_percpu_gfp().
    pub fn alloc_gfp(_gfp: c::gfp_t) -> Result<Self, AllocError> {
        let p = alloc_percpu(size_of::<T>(), align_of::<T>());
        if p.is_null() { Err(AllocError) } else { Ok(PerCpu(p.cast())) }
    }

    /// alloc(), as a field's initializer.
    pub fn init() -> impl ::pin_init::Init<Self, AllocError> {
        // SAFETY: the slot written, or nothing done
        unsafe {
            ::pin_init::init_from_closure(|slot: *mut Self| {
                slot.write(Self::alloc()?);
                Ok(())
            })
        }
    }
}


/// alloc_init()'s initializer, of @copy from zeroes, for CPU @cpu.
pub type InitCopy<T, C> = unsafe extern "C" fn(copy: *mut T, ctx: *mut C, cpu: c_uint);

impl<T> PerCpu<T> {
    /// Every possible CPU's copy, initialized by @init: bch2_alloc_percpu_init().
    /// In userspace a CPU is a thread's slot, and a thread that starts later
    /// is a CPU coming online - @init runs on its copy then, until the PerCpu
    /// is freed.
    ///
    /// # Safety
    /// @init only initializes the copy it's given, from zeroes; @ctx lives
    /// until the PerCpu is freed.
    pub unsafe fn alloc_init<C>(init: InitCopy<T, C>, ctx: *mut C) -> Result<Self, AllocError> {
        let p = PerCpu::<MaybeUninit<T>>::alloc()?.into_raw().cast::<T>();
        let init = unsafe { core::mem::transmute::<InitCopy<T, C>, InitCopy<c_void, c_void>>(init) };
        let ctx = ctx.cast();

        let mut s = lock();
        for cpu in possible_cpus() {
            unsafe { init(per_cpu_ptr(p, cpu).cast(), ctx, cpu) };
        }
        s.dynamic_inits.push(DynamicInit { pcv: p.addr(), init, ctx });
        Ok(PerCpu(p))
    }
}

/// @n u64 counters, percpu, of a length known only when they're allocated:
/// freed when dropped.
pub struct PerCpuU64s {
    p: NonNull<u64>,
    n: usize,
}

impl PerCpuU64s {
    pub fn new(n: usize) -> Result<Self, AllocError> {
        let p = alloc_percpu(n * size_of::<u64>(), align_of::<u64>());
        Ok(Self { p: NonNull::new(p.cast()).ok_or(AllocError)?, n })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn iter(&self) -> impl Iterator<Item = PerCpuRef<'_, u64>> + '_ {
        (0..self.n).map(move |i| unsafe { PerCpuRef::from_raw(self.p.as_ptr().add(i)) })
    }

    /// Each counter set to @v's, zero past its end: through &mut, so no add
    /// races it.
    pub fn set(&mut self, v: &[u64]) {
        for (i, c) in self.iter().enumerate() {
            unsafe { c.set(v.get(i).copied().unwrap_or(0)) };
        }
    }
}

impl Drop for PerCpuU64s {
    fn drop(&mut self) {
        unsafe { free_percpu(self.p.as_ptr().cast()) }
    }
}

impl<T> PerCpu<T> {
    /// Whether it's been allocated - and not freed.
    pub fn allocated(&self) -> bool {
        !self.0.is_null()
    }

    /// Its pointer, for a `T __percpu *` to own: freed with free_percpu().
    pub fn into_raw(self) -> *mut T {
        self.0
    }

    /// free_percpu(): unallocated after, as before alloc().
    pub fn free(&mut self) {
        unsafe { free_percpu(self.0.cast()) };
        self.0 = core::ptr::null_mut();
    }

    pub fn get(&self) -> PerCpuRef<'_, T> {
        PerCpuRef(self.0, PhantomData)
    }

    pub fn this_cpu<'g>(&'g self, g: &'g impl Pinned) -> ThisCpu<'g, T> {
        self.get().this_cpu(g)
    }
}

impl<T, const N: usize> PerCpu<[T; N]> {
    pub fn at(&self, i: usize) -> PerCpuRef<'_, T> {
        self.get().at(i)
    }
}

/// How far into a @T the field @f projects to is: in a T on the stack, never
/// read.
fn field_offset<T, U>(f: impl FnOnce(*const T) -> *const U) -> usize {
    let t = MaybeUninit::<T>::uninit();
    f(t.as_ptr()) as usize - t.as_ptr() as usize
}

/// A field of a PerCpuRef or ThisCpu: percpu_field!(c.capacity.pcpu.get(),
/// online_reserved[slot]).
#[macro_export]
macro_rules! percpu_field {
    ($p:expr, $($f:tt)+) => {{
        let p = $p;
        // SAFETY: a field of *t, projected without reading it
        unsafe { p.field(|t| ::core::ptr::addr_of!((*t).$($f)+)) }
    }};
}

impl<'a, T> PerCpuRef<'a, T> {
    /// A C `T __percpu *` that isn't a PerCpu: a table's entry's counters.
    ///
    /// # Safety
    /// @p is a percpu allocation of a @T, live for 'a.
    pub unsafe fn from_raw(p: *mut T) -> Self {
        PerCpuRef(p, PhantomData)
    }

    /// percpu_field!()'s.
    ///
    /// # Safety
    /// @f only projects @t to a field of it, reading nothing.
    pub unsafe fn field<U>(self, f: impl FnOnce(*const T) -> *const U) -> PerCpuRef<'a, U> {
        PerCpuRef(self.0.wrapping_byte_add(field_offset(f)).cast(), PhantomData)
    }

    /// Each possible CPU's copy.
    #[inline]
    pub fn cpus(self) -> impl Iterator<Item = CpuCopy<'a, T>> {
        possible_cpus().map(move |cpu| CpuCopy(per_cpu_ptr(self.0, cpu), PhantomData))
    }

    pub fn this_cpu<'g>(self, g: &'g impl Pinned) -> ThisCpu<'g, T>
        where 'a: 'g
    {
        ThisCpu(unsafe { this_cpu_ptr(self.0, g) }, PhantomData)
    }

    /// Nothing else may reach it meanwhile, on any CPU.
    pub unsafe fn zero(self) where T: Default {
        for cpu in self.cpus() {
            unsafe { cpu.0.cast_mut().write(T::default()) };
        }
    }
}

// SAFETY: a zeroed local_lock_t is a valid uninitialized one, as in C's
// alloc_percpu()'d struct; init() below initializes it
unsafe impl ::pin_init::Zeroable for Opaque<c::local_lock_t> {}

impl<'a> PerCpuRef<'a, Opaque<c::local_lock_t>> {
    /// Each CPU's initialized: local_lock_init(), before any takes it.
    pub fn init(self) {
        for cpu in self.cpus() {
            // SAFETY: a CPU's copy, which no one takes till it's initialized
            unsafe { rust_local_lock_init(cpu.0.cast_mut().cast()) }
        }
    }

    /// This CPU's, taken: local_lock_irqsave().
    pub fn lock_irqsave(self) -> LocalLockGuard<'a> {
        let pcp = self.0.cast::<c::local_lock_t>();
        // SAFETY: live for 'a, and initialized
        let flags = unsafe { rust_local_lock_irqsave(pcp) };
        LocalLockGuard { pcp, flags, _noreclaim: MemallocFlags::noreclaim(), _p: PhantomData }
    }
}

impl<'a, T, const N: usize> PerCpuRef<'a, [T; N]> {
    pub fn at(self, i: usize) -> PerCpuRef<'a, T> {
        assert!(i < N);
        PerCpuRef(self.0.cast::<T>().wrapping_add(i), PhantomData)
    }
}

impl<T: Counters> PerCpu<T> {
    pub fn sum(&self) -> T {
        self.get().sum()
    }

    pub fn sum_nonnegative(&self) -> T {
        self.get().sum_nonnegative()
    }
}

impl<T: Counters> PerCpuRef<'_, T> {
    /// Exact only with the adders excluded.
    pub fn sum(&self) -> T {
        let mut cpus = self.cpus().map(|c| c.get());
        let first = cpus.next().expect("CPU 0 is always possible");
        cpus.fold(first, T::wrapping_add)
    }

    /// For a read that races the adds.
    pub fn sum_nonnegative(&self) -> T {
        self.sum().clamp_nonnegative()
    }
}

impl<T: PerCpuWord + From<u8>> PerCpuRef<'_, T> {
    /// This CPU's, or the one the task was just on: for a cmpxchg() to start
    /// from, which fails unless it's this CPU's value now.
    #[inline(always)]
    pub fn read(&self) -> T {
        unsafe { T::this_cpu_read(self.0) }
    }

    /// A subtraction is the add of its wrapping_neg().
    #[inline(always)]
    pub fn add(&self, v: T) {
        unsafe { T::this_cpu_add(self.0, v) }
    }

    /// This CPU's to @v, every other's to 0.
    ///
    /// # Safety
    /// An add racing it may or may not count. Whether that's sound is up to
    /// what the variable is for - or there are no adders yet.
    pub unsafe fn set(&self, v: T) {
        for cpu in possible_cpus() {
            // SAFETY: a live CPU's copy; another CPU's, written as read
            unsafe { core::ptr::write_volatile(per_cpu_ptr(self.0, cpu), T::from(0)) };
        }
        self.xchg(v);
    }

    #[inline(always)]
    pub fn xchg(&self, v: T) -> T {
        let mut old = self.read();
        loop {
            match self.cmpxchg(old, v) {
                Ok(old) => return old,
                Err(cur) => old = cur,
            }
        }
    }

    /// Err(what it was), if it wasn't @old.
    #[inline(always)]
    pub fn cmpxchg(&self, old: T, new: T) -> Result<T, T> {
        match unsafe { T::this_cpu_cmpxchg(self.0, old, new) } {
            v if v == old => Ok(v),
            v             => Err(v),
        }
    }
}

/// A per-CPU T any CPU may hold a shared reference to: every part another
/// CPU writes atomic - a Volatile - or behind an UnsafeCell - Opaque.
///
/// # Safety
/// A &T races nothing another CPU does to its copy.
pub unsafe trait CpuShared {}

// SAFETY: every access atomic
unsafe impl<T: AtomicWord> CpuShared for Volatile<T> {}
// SAFETY: each element's
unsafe impl<T: CpuShared, const N: usize> CpuShared for [T; N] {}
// SAFETY: an UnsafeCell's
unsafe impl<T> CpuShared for Opaque<T> {}

impl<'a, T: CpuShared> PerCpuRef<'a, T> {
    /// Each possible CPU's copy: slots, anyone's to load and store.
    pub fn each_cpu(self) -> impl Iterator<Item = &'a T> {
        // SAFETY: a live CPU's copy, for 'a; CpuShared, so a shared
        // reference races nothing
        possible_cpus().map(move |cpu| unsafe { &*per_cpu_ptr(self.0, cpu) })
    }

    /// This CPU's copy - or by the time it's used, the one this was on:
    /// raw_cpu_ptr(), for a copy any CPU may use.
    pub fn raw_this_cpu(self) -> &'a T {
        let g = CpuGuard::new();
        // SAFETY: as each_cpu()'s, of the CPU @g pinned us to
        unsafe { &*this_cpu_ptr(self.0, &g) }
    }
}

impl<'a> PerCpuRef<'a, Volatile<u64>> {
    /// The slot as a word: its read() and cmpxchg(), this CPU's.
    pub fn word(self) -> PerCpuRef<'a, u64> {
        // Volatile<u64> is laid out as a u64 (repr(transparent))
        PerCpuRef(self.0.cast(), PhantomData)
    }
}

/// One CPU's copy, from PerCpuRef::cpus(): see the top of the file.
pub struct CpuCopy<'a, T>(*const T, PhantomData<&'a PerCpu<T>>);

impl<T> Clone for CpuCopy<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for CpuCopy<'_, T> {}

impl<'a, T> CpuCopy<'a, T> {
    /// percpu_field!()'s.
    ///
    /// # Safety
    /// @f only projects @t to a field of it, reading nothing.
    #[inline]
    pub unsafe fn field<U>(self, f: impl FnOnce(*const T) -> *const U) -> CpuCopy<'a, U> {
        CpuCopy(self.0.wrapping_byte_add(field_offset(f)).cast(), PhantomData)
    }

    /// Its CPU's copy, as a pointer: for its owner to initialize, or tear
    /// down.
    pub fn as_ptr(self) -> *const T {
        self.0
    }

    /// Its CPU's copy, by reference: for its owner while no CPU uses it - its
    /// teardown.
    ///
    /// # Safety
    /// Nothing else reaches it while 'a lasts, here or on any CPU.
    pub unsafe fn as_mut(self) -> &'a mut T {
        unsafe { &mut *self.0.cast_mut() }
    }
}

impl<T: Copy> CpuCopy<'_, T> {
    #[inline]
    pub fn get(&self) -> T {
        // SAFETY: a live CPU's copy, written as it's read
        unsafe { core::ptr::read_volatile(self.0) }
    }
}

/// This CPU's copy, as a place: see the top of the file.
/// Not Send or Sync, as its guard isn't.
pub struct ThisCpu<'g, T>(*mut T, PhantomData<(&'g (), *const ())>);

impl<T> Clone for ThisCpu<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ThisCpu<'_, T> {}

impl<'g, T> ThisCpu<'g, T> {
    /// percpu_field!()'s.
    ///
    /// # Safety
    /// @f only projects @t to a field of it, reading nothing.
    pub unsafe fn field<U>(self, f: impl FnOnce(*const T) -> *const U) -> ThisCpu<'g, U> {
        ThisCpu(self.0.wrapping_byte_add(field_offset(f)).cast(), PhantomData)
    }

    /// This CPU's copy, by reference: for a T only its own CPU touches, which
    /// the guard keeps the rest of this CPU out of - an IrqGuard, for one
    /// interrupt handlers change.
    ///
    /// # Safety
    /// Nothing else reaches it while 'g lasts: no other CPU, nothing the guard
    /// lets in on this one, and no other ThisCpu of it.
    pub unsafe fn as_mut(self) -> &'g mut T {
        unsafe { &mut *self.0 }
    }
}

impl<'g, T: CpuShared> core::ops::Deref for ThisCpu<'g, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this CPU's copy, live while the guard is; CpuShared, so a
        // shared reference races nothing
        unsafe { &*self.0 }
    }
}

impl<'g, T, const N: usize> ThisCpu<'g, [T; N]> {
    pub fn at(self, i: usize) -> ThisCpu<'g, T> {
        assert!(i < N);
        ThisCpu(self.0.cast::<T>().wrapping_add(i), PhantomData)
    }
}

impl<T: Copy> ThisCpu<'_, T> {
    pub fn get(&self) -> T {
        unsafe { core::ptr::read_volatile(self.0) }
    }

    pub fn set(&self, v: T) {
        unsafe { core::ptr::write_volatile(self.0, v) }
    }

    /// As a place, to read-modify-write: `*x.place() += 1`.
    pub fn place(&self) -> FakePlaceRef<'_, Self> {
        FakePlaceWrite::place(self)
    }
}

impl<T: Counters> ThisCpu<'_, T> {
    /// Not atomic as a whole: a load and a store of the struct.
    pub fn add(&self, delta: T) {
        self.set(self.get().wrapping_add(delta))
    }
}

impl<T: Copy> FakePlaceRead for ThisCpu<'_, T> {
    type Value = T;

    fn get(&self) -> T {
        ThisCpu::get(self)
    }
}

impl<T: Copy> FakePlaceWrite for ThisCpu<'_, T> {
    fn set(&self, v: T) {
        ThisCpu::set(self, v)
    }
}

// ── Counters ─────────────────────────────────────────────────────────────

/// How a counter, or a struct or array of them, sums: see the top of the
/// file. A struct gets it with #[fieldwise(Counters::wrapping_add(rhs),
/// Counters::clamp_nonnegative)].
pub trait Counters: Copy {
    fn wrapping_add(self, rhs: Self) -> Self;
    fn clamp_nonnegative(self) -> Self;
}

macro_rules! counters {
    ($($t:ty, $signed:ty);*) => { $(
        impl Counters for $t {
            #[inline]
            fn wrapping_add(self, rhs: $t) -> $t {
                <$t>::wrapping_add(self, rhs)
            }

            #[inline]
            fn clamp_nonnegative(self) -> $t {
                (self as $signed).max(0) as $t
            }
        }
    )* };
}
counters!(u32, i32; u64, i64);

impl<T: Counters, const N: usize> Counters for [T; N] {
    #[inline]
    fn wrapping_add(self, rhs: Self) -> Self {
        core::array::from_fn(|i| self[i].wrapping_add(rhs[i]))
    }

    #[inline]
    fn clamp_nonnegative(self) -> Self {
        self.map(Counters::clamp_nonnegative)
    }
}
