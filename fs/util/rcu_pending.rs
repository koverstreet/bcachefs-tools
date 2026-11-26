// SPDX-License-Identifier: GPL-2.0

//! An allocator of T's, over A: what's freed waits out an RCU or SRCU grace
//! period - per CPU, batched by the grace period it waits for - before it
//! goes back to A, and is handed out again before then if T says it can be.
//!
//! What's freed is dropped as T says: at its free, if nothing a reader
//! reaches goes with it - DropAnytime - or else once its grace period is
//! over, here. So the Allocator, whose users write a T over what allocate()
//! gives and drop it before deallocate(), is only for a DropAnytime T; for
//! any other, alloc() gives a T and free() takes one undropped.

use core::alloc::Layout;
use core::ffi::c_uint;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{offset_of, MaybeUninit};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use pin_init::{pin_init_from_closure, PinInit, Zeroable};

use crate::c;
use crate::c::rcu_head;
use crate::errcode::BchError;
use crate::locking::{Locked, SpinLockIrq};
use crate::new_lock;
use crate::types::GenRadix;
use crate::util::alloc::{flags, init_at, Alloc, AllocError, AllocGfp, Allocator, Flags, Vec};
use crate::util::ffi::Opaque;
use crate::data_structures::genradix::NewNode;
use crate::os::percpu::{free_percpu, CpuShared, PerCpu, PerCpuRef};
use crate::util::maybe_rcu_alloc::RcuFreeing;
use crate::util::rcu::{Flavor, GpCookie, HasRcuHead, RcuHead, RcuOwned};
use crate::os::workqueue::{Queue, RawWork};

const ACTIVE: usize = crate::os::rcu::NUM_ACTIVE_POLL_OLDSTATE;

/// A T that may be handed out again - as a T, by RcuPending - while readers
/// from before its free may still be in it.
///
/// # Safety
/// Those readers are fine with what they find: the T, or what init() made of
/// fresh memory, until a grace period after its free.
pub unsafe trait RcuRealloc: HasRcuHead + Sized {
    /// Whether this one, freed, can be reused yet: if true, it's claimed.
    /// Called under the CPU's lock: mustn't sleep.
    fn try_claim(&self) -> bool;

    /// A T made of memory fresh from the allocator, as a freed one is: alloc()
    /// claims it.
    fn init() -> impl PinInit<Self, BchError>;
}

/// An item, given up: its rcu_head, what the lists thread through.
type Raw = NonNull<rcu_head>;

fn into_raw<T: HasRcuHead>(p: NonNull<T>) -> Raw {
    unsafe { p.byte_add(T::HEAD).cast() }
}

/// # Safety
/// @raw is into_raw()'s.
unsafe fn from_raw<T: HasRcuHead>(raw: Raw) -> NonNull<T> {
    unsafe { raw.byte_sub(T::HEAD).cast() }
}

/// @h's link: the kernel's next, or liburcu's next.next.
fn link(h: NonNull<rcu_head>) -> *mut Option<NonNull<rcu_head>> {
    #[cfg(kernel)]
    return unsafe { (&raw mut (*h.as_ptr()).next).cast() };
    #[cfg(not(kernel))]
    return unsafe { (&raw mut (*h.as_ptr()).next.next).cast() };
}

/// A list of queued items, through their heads - the list's while they're on
/// it.
#[derive(Default)]
struct List {
    head: Option<NonNull<rcu_head>>,
    tail: Option<NonNull<rcu_head>>,
    seq:  GpCookie,
}

impl List {
    fn add(&mut self, n: NonNull<rcu_head>) {
        match self.tail {
            None    => self.head = Some(n),
            Some(t) => unsafe { *link(t) = Some(n) },
        }
        self.tail = Some(n);
        unsafe { *link(n) = None };
    }

    fn merge(&mut self, l2: &mut List) {
        match self.tail {
            None    => self.head = l2.head,
            Some(t) => unsafe { *link(t) = l2.head },
        }
        self.tail = l2.tail;
        l2.head = None;
        l2.tail = None;
    }

    /// Emptied, its entries in order: each one's link read before it's
    /// handed out, so what's done with it may free it.
    fn drain(&mut self) -> Drain {
        self.tail = None;
        Drain(self.head.take())
    }

    /// The first entry @f is true of, unlinked.
    fn remove_where(&mut self, mut f: impl FnMut(NonNull<rcu_head>) -> bool) -> Option<NonNull<rcu_head>> {
        let mut prev = None;
        let mut cur = self.head;

        while let Some(c) = cur {
            let next = unsafe { *link(c) };

            if f(c) {
                match prev {
                    Some(p) => unsafe { *link(p) = next },
                    None    => self.head = next,
                }
                if self.tail == Some(c) {
                    self.tail = prev;
                }
                return Some(c);
            }

            prev = cur;
            cur = next;
        }
        None
    }
}

struct Drain(Option<NonNull<rcu_head>>);

impl Iterator for Drain {
    type Item = NonNull<rcu_head>;

    fn next(&mut self) -> Option<NonNull<rcu_head>> {
        let h = self.0?;
        self.0 = unsafe { *link(h) };
        Some(h)
    }
}

/// The items waiting for one grace period.
struct Seq {
    /*
     * We're using a radix tree like a vector - we're just pushing elements
     * onto the end; we're using a radix tree instead of an actual vector to
     * avoid reallocation overhead
     */
    objs:   GenRadix<Option<Raw>>,
    nr:     usize,
    seq:    GpCookie,
}
crate::genradix_entry!(Option<Raw>);

impl Seq {
    fn new(seq: GpCookie) -> Self {
        Seq { objs: Zeroable::zeroed(), nr: 0, seq }
    }

    fn get(&self, i: usize) -> Raw {
        self.objs.get(i).copied().flatten().expect("a seq's entry, below its nr")
    }

    fn iter(&self) -> impl Iterator<Item = Raw> + '_ {
        (0..self.nr).map(|i| self.get(i))
    }

    /// Onto the end: false if that needs a node, and there's none to be had
    /// atomically - nor in @node.
    fn push(&mut self, raw: Raw, node: &mut NewNode<Option<Raw>>) -> bool {
        match self.objs.get_or_alloc_preallocated(self.nr, node, flags::ATOMIC | flags::NOWARN) {
            Ok(slot) => {
                *slot = Some(raw);
                self.nr += 1;
                true
            }
            Err(_) => false,
        }
    }

    /// The newest entry @f is true of, taken out.
    fn remove_where(&mut self, f: &mut impl FnMut(Raw) -> bool) -> Option<Raw> {
        let i = (0..self.nr).rev().find(|&i| f(self.get(i)))?;
        let ret = self.get(i);

        /* Shift later entries down to close the gap. */
        for j in i..self.nr - 1 {
            let next = self.get(j + 1);
            *self.objs.get_mut(j).expect("a seq's entry, below its nr") = Some(next);
        }

        self.nr -= 1;
        if self.nr == 0 {
            self.objs.free();
        }
        Some(ret)
    }
}

impl Drop for Seq {
    fn drop(&mut self) {
        self.objs.free()
    }
}

struct PcpuState {
    /*
     * We can't bound the number of unprocessed gp sequence numbers, and we
     * can't efficiently merge radix trees for expired grace periods, so we
     * need darray/vector:
     */
    objs:     Vec<Seq>,

    /* Third entry is for expired objects: */
    lists:    [List; ACTIVE + 1],

    cb_armed: bool,
}

// pointers to what's queued, and into its own radix trees: anywhere, under
// its lock
unsafe impl Send for PcpuState {}

/// What's expired, taken out under the lock to process outside it.
struct Expired {
    objs: Option<Seq>,
    list: Drain,
}

impl PcpuState {
    /// Items, not Seqs: one can be left empty, by a claim, or by an
    /// enqueue that made it and then found its item's grace period over -
    /// with nothing armed to process it, which counting it would wait on
    /// forever. take_expired() drops it once it's first and expired.
    fn has_pending(&self) -> bool {
        self.objs.iter().any(|o| o.nr != 0) || self.lists.iter().any(|l| l.head.is_some())
    }

    fn merge_expired_lists(&mut self, srcu: Flavor<'_>) {
        let (active, expired) = self.lists.split_at_mut(ACTIVE);
        for i in active {
            if i.head.is_some() && srcu.poll(i.seq) {
                expired[0].merge(i);
            }
        }
    }

    /// The first Seq, if its grace period is over, and the expired lists:
    /// None if there's nothing to process.
    fn take_expired(&mut self, srcu: Flavor<'_>) -> Option<Expired> {
        /*
         * XXX: we should grab the gp seq once and avoid multiple function
         * calls, this is called from __rcu_pending_enqueue() fastpath in
         * may_sleep==true mode
         */
        let first = self.objs.first().is_some_and(|o| srcu.poll(o.seq));
        if !first &&
           !self.lists[..ACTIVE].iter().any(|l| l.head.is_some() && srcu.poll(l.seq)) &&
           self.lists[ACTIVE].head.is_none() {
            return None;
        }

        let objs = if first {
            self.objs.rotate_left(1);
            self.objs.pop()
        } else {
            None
        };

        self.merge_expired_lists(srcu);
        Some(Expired { objs, list: self.lists[ACTIVE].drain() })
    }

    fn get_object_radix(&mut self, seq: GpCookie) -> Option<&mut Seq> {
        if let Some(i) = self.objs.iter().rposition(|o| o.seq == seq) {
            return Some(&mut self.objs[i]);
        }
        self.objs.push_gfp(Seq::new(seq), flags::ATOMIC).ok()?;
        self.objs.last_mut()
    }
}

#[repr(C)]
struct Pcpu<'f, T: RcuRealloc, A: Alloc> {
    parent: *const RcuPending<'f, T, A>,
    lock:   Locked<SpinLockIrq, PcpuState>,
    /* what's queued here: added under the lock, taken off as it's processed */
    nr:     AtomicUsize,
    cpu:    c_uint,
    cb:     Opaque<rcu_head>,
    work:   RawWork,
}

/// A CPU's one callback, armed: whoever set cb_armed owns its head, until the
/// callback finds nothing pending and clears it.
struct Armed<'f, T: RcuRealloc, A: Alloc>(NonNull<Pcpu<'f, T, A>>);

// a CPU's state, which its lock serializes, the callback's on any CPU
unsafe impl<T: RcuRealloc, A: Alloc> Send for Armed<'_, T, A> {}

// cb_armed is the head's: set, it's the Armed's alone
unsafe impl<T: RcuRealloc, A: Alloc> RcuOwned for Armed<'_, T, A> {
    fn into_head(self) -> NonNull<RcuHead> {
        unsafe { NonNull::new_unchecked((*self.0.as_ptr()).cb.as_ptr()).cast() }
    }

    unsafe fn from_head(head: NonNull<RcuHead>) -> Self {
        Armed(unsafe { head.byte_sub(offset_of!(Pcpu<'_, T, A>, cb)).cast() })
    }
}

impl<T: RcuRealloc, A: Alloc> Armed<'_, T, A> {
    /// A grace period's passed: the CPU's expired items processed, and the
    /// callback armed again while any are still waiting.
    fn run(self) {
        // a Pcpu outlives its RcuPending's barrier(), which waits for this
        let p = unsafe { self.0.as_ref() };
        let pending = unsafe { &*p.parent };

        unsafe { p.work.queue_on(Queue::system(), p.cpu) };

        let mut s = p.lock.lock();
        if s.has_pending() {
            drop(s);
            pending.srcu.call(self, Self::run);
        } else {
            s.cb_armed = false;
        }
    }
}

// what other CPUs change is under the lock, or C's - cb and work
unsafe impl<T: RcuRealloc, A: Alloc> CpuShared for Pcpu<'_, T, A> {}

/// Pinned: each CPU's state points back to it.
pub struct RcuPending<'f, T: RcuRealloc, A: Alloc> {
    p:     *mut Pcpu<'f, T, A>,
    srcu:  Flavor<'f>,
    /* written by init(), dropped by free_pcpu(): there while p is */
    alloc: MaybeUninit<A>,
    _p:    PhantomData<(fn() -> T, PhantomPinned)>,
}

// what's pending is T's, handed out on any CPU
unsafe impl<T: RcuRealloc + Send, A: Alloc + Send> Send for RcuPending<'_, T, A> {}
unsafe impl<T: RcuRealloc + Send, A: Alloc + Sync> Sync for RcuPending<'_, T, A> {}

impl<'f, T: RcuRealloc, A: Alloc> RcuPending<'f, T, A> {
    /// With @srcu's grace periods, over @alloc.
    pub fn new(srcu: Flavor<'f>, alloc: A) -> impl PinInit<Self, AllocError> {
        unsafe { pin_init_from_closure(move |slot: *mut Self| Self::init(slot, srcu, alloc)) }
    }

    fn pcpu(&self) -> PerCpuRef<'_, Pcpu<'f, T, A>> {
        unsafe { PerCpuRef::from_raw(self.p) }
    }

    fn backing(&self) -> &A {
        unsafe { self.alloc.assume_init_ref() }
    }

    /// An item whose grace period is over, back to A: dropped first, if it
    /// wasn't at its free.
    fn process_raw(&self, raw: Raw) {
        let p = unsafe { from_raw::<T>(raw) };
        if !<T as RcuFreeing>::drops_at_free() {
            unsafe { ptr::drop_in_place(p.as_ptr()) };
        }
        unsafe { self.backing().deallocate(p.cast(), Layout::new::<T>()) }
    }

    /// What take_expired() took out of @p, processed - with the lock dropped.
    fn process_expired(&self, p: &Pcpu<'f, T, A>, e: Expired) {
        for raw in e.objs.iter().flat_map(Seq::iter) {
            self.process_raw(raw);
            p.nr.fetch_sub(1, Relaxed);
        }
        drop(e.objs);

        for h in e.list {
            self.process_raw(h);
            p.nr.fetch_sub(1, Relaxed);
        }
    }

    unsafe extern "C" fn work_fn(work: *mut c::work_struct) {
        let p = unsafe { &*work.byte_sub(offset_of!(Pcpu<'f, T, A>, work)).cast::<Pcpu<'f, T, A>>() };
        let pending = unsafe { &*p.parent };

        loop {
            let e = p.lock.lock().take_expired(pending.srcu);
            let Some(e) = e else { break };
            pending.process_expired(p, e);
        }
    }

    /// Onto a list, the radix being out of reach: whether the list is new,
    /// and wants a grace period started. None if the item's grace period is
    /// over, for it to be processed with the lock dropped.
    fn enqueue_list(&self, s: &mut PcpuState, seq: GpCookie, raw: Raw) -> Option<bool> {
        let i = loop {
            if let Some(i) = s.lists[..ACTIVE].iter().position(|l| l.seq == seq) {
                break i;
            }

            if let Some(i) = s.lists[..ACTIVE].iter().position(|l| l.head.is_none()) {
                break i;
            }

            s.merge_expired_lists(self.srcu);

            /*
             * Both lists still waiting, on grace periods neither ours: then
             * ours is over - no more than ACTIVE are pending at once. It can
             * be, having dropped the lock since we took @seq: don't wait here
             * for one of theirs, which interrupts off may keep from ending.
             */
            if self.srcu.poll(seq) {
                return None;
            }
        };

        /*
         * Empty, it wants a grace period started - as an empty Seq does -
         * even with our seq: a claim may have emptied it, and the callback
         * have found nothing pending since and disarmed
         */
        let l = &mut s.lists[i];
        let new = l.head.is_none();
        l.seq = seq;
        l.add(raw);
        Some(new)
    }

    /// Queue @p, to be processed once its grace period has elapsed.
    ///
    /// Attempt to enqueue items onto a radix tree; if memory allocation fails,
    /// fall back to a linked list. Will do GFP_KERNEL memory allocations, and
    /// process expired items.
    fn enqueue(&self, p: NonNull<T>) {
        let raw = into_raw(p);
        let mut may_sleep = true;
        let mut node = NewNode::none();

        /* We could technically be scheduled before taking the lock and end up
         * using a different cpu's rcu_pending_pcpu: that's ok, it needs a lock
         * anyways
         *
         * And we have to do it this way to avoid breaking PREEMPT_RT, which
         * redefines how spinlocks work:
         */
        let mut p = self.pcpu().raw_this_cpu();
        let mut lock = Some(p.lock.lock());
        let seq = self.srcu.get_state();

        'retry: loop {
            let s = match &mut lock {
                Some(s) => s,
                None => {
                    /* check_expired: */
                    if self.srcu.poll(seq) {
                        self.process_raw(raw);
                        return;
                    }

                    p = self.pcpu().raw_this_cpu();
                    lock.insert(p.lock.lock())
                }
            };

            if may_sleep {
                if let Some(e) = s.take_expired(self.srcu) {
                    lock = None;
                    self.process_expired(p, e);
                    continue 'retry;
                }
            }

            let start_gp = 'add: {
                if let Some(objs) = s.get_object_radix(seq) {
                    let start = objs.nr == 0;
                    if objs.push(raw, &mut node) {
                        break 'add start;
                    }

                    /*
                     * New radix tree nodes must be added under @p->lock because the
                     * tree root is in a darray that can be resized (typically,
                     * genradix supports concurrent unlocked allocation of new
                     * nodes) - hence preallocation and the retry loop:
                     */
                    if may_sleep {
                        lock = None;
                        may_sleep = node.alloc().is_ok();
                        continue 'retry;
                    }
                }

                match self.enqueue_list(s, seq, raw) {
                    Some(start) => start,
                    None => {
                        lock = None;
                        continue 'retry;
                    }
                }
            };
            p.nr.fetch_add(1, Relaxed);

            if start_gp {
                /*
                 * We only have one callback (ideally, we would have one for
                 * every outstanding graceperiod) - so if our callback is
                 * already in flight, we may still have to start a grace period
                 * (since we used get_state() above, not start_poll())
                 */
                if !s.cb_armed {
                    s.cb_armed = true;
                    self.srcu.call(Armed(NonNull::from(p)), Armed::run);
                } else {
                    self.srcu.start_poll();
                }
            } else {
                // what's pending on a CPU has its callback armed
                debug_assert!(s.cb_armed, "rcu_pending: queued behind items with no callback armed");
            }
            return;
        }
    }

    /// The first of @p's items T::try_claim() claims, dequeued: most recent
    /// radix-tree entries first, then oldstate lists.
    fn claim(&self, p: &Pcpu<'f, T, A>) -> Option<NonNull<T>> {
        let mut claimed = |raw: Raw| unsafe { from_raw::<T>(raw).as_ref() }.try_claim();
        let mut s = p.lock.lock();

        let raw = match s.objs.iter_mut().find_map(|o| o.remove_where(&mut claimed)) {
            Some(raw) => raw,
            None => s.lists.iter_mut().find_map(|l| l.remove_where(&mut claimed))?,
        };
        p.nr.fetch_sub(1, Relaxed);
        Some(unsafe { from_raw(raw) })
    }

    /// A T: one freed and claimed from this CPU, else fresh from A, made by
    /// its init(), else one claimed from any CPU.
    pub fn alloc(&self) -> Result<NonNull<T>, AllocError> {
        self.alloc_from(|a| a.allocate_zeroed(Layout::new::<T>()))
    }

    /// alloc(), its fresh memory @fresh's.
    fn alloc_from(&self, fresh: impl FnOnce(&A) -> Result<NonNull<[u8]>, AllocError>)
        -> Result<NonNull<T>, AllocError>
    {
        if let Some(p) = self.claim(self.pcpu().raw_this_cpu()) {
            return Ok(p);
        }

        if let Ok(p) = fresh(self.backing()) {
            let p = p.cast::<T>();
            if unsafe { init_at(T::init(), p.as_ptr()) }.is_ok() {
                assert!(unsafe { p.as_ref() }.try_claim(), "a fresh T, its claim refused");
                return Ok(p);
            }
            unsafe { self.backing().deallocate(p.cast(), Layout::new::<T>()) };
        }

        self.pcpu().each_cpu().find_map(|p| self.claim(p)).ok_or(AllocError)
    }

    /// The Allocator's alloc(): @layout, if it's within T's.
    fn allocate_from(&self, layout: Layout, fresh: impl FnOnce(&A) -> Result<NonNull<[u8]>, AllocError>)
        -> Result<NonNull<[u8]>, AllocError>
    {
        <T as RcuFreeing>::assert_drops_at_free();

        let t = Layout::new::<T>();
        if layout.size() > t.size() || layout.align() > t.align() {
            return Err(AllocError);
        }
        Ok(NonNull::slice_from_raw_parts(self.alloc_from(fresh)?.cast(), t.size()))
    }

    /// @p freed: dropped now, if T is DropAnytime, else once its grace period
    /// is over - its memory handed out again, or back to A, after that.
    ///
    /// # Safety
    /// @p is a T in memory from A - alloc()'s, or another RcuPending's over
    /// the same A - and nothing but readers from before this uses it after.
    pub unsafe fn free(&self, p: NonNull<T>) {
        if <T as RcuFreeing>::drops_at_free() {
            unsafe { ptr::drop_in_place(p.as_ptr()) };
        }
        self.enqueue(p)
    }

    /// How many items are queued, not yet processed or claimed.
    pub fn nr_pending(&self) -> usize {
        self.pcpu().each_cpu().map(|p| p.nr.load(Relaxed)).sum()
    }

    fn has_pending_or_armed(&self) -> bool {
        self.pcpu().each_cpu().any(|p| {
            let s = p.lock.lock();
            s.has_pending() || s.cb_armed
        })
    }

    unsafe extern "C" fn init_pcpu(slot: *mut Pcpu<'f, T, A>, this: *mut Self, cpu: c_uint) {
        let lock = new_lock!(PcpuState {
            objs:     Vec::new(),
            lists:    Default::default(),
            cb_armed: false,
        }, "rcu_pending_pcpu.lock");
        let work = crate::new_work!(Self::work_fn, "rcu_pending_pcpu.work");
        unsafe {
            (&raw mut (*slot).parent).write(this);
            (&raw mut (*slot).nr).write(AtomicUsize::new(0));
            (&raw mut (*slot).cpu).write(cpu);
            let Ok(()) = init_at(lock, &raw mut (*slot).lock);
            let Ok(()) = init_at(work, &raw mut (*slot).work);
        }
    }

    /// new(), at @this.
    ///
    /// # Safety
    /// @this doesn't move until it's freed.
    unsafe fn init(this: *mut Self, srcu: Flavor<'f>, alloc: A) -> Result<(), AllocError> {
        let p = unsafe { PerCpu::alloc_init(Self::init_pcpu, this) }?.into_raw();

        unsafe {
            (&raw mut (*this).p).write(p);
            (&raw mut (*this).srcu).write(srcu);
            (&raw mut (*this).alloc).write(MaybeUninit::new(alloc));
        }
        Ok(())
    }

    /// Everything queued waited for, and processed - with what processing's
    /// queued run.
    fn drain(&self) {
        while self.has_pending_or_armed() {
            self.srcu.barrier();

            for p in self.pcpu().each_cpu() {
                p.work.flush();
            }
        }

        for p in self.pcpu().each_cpu() {
            p.work.flush();
        }
    }

    /// # Safety
    /// drain()ed, and nothing enqueues after: exit()'s @this.
    unsafe fn free_pcpu(this: *mut Self) {
        for p in unsafe { &*this }.pcpu().cpus() {
            let p = p.as_ptr().cast_mut();
            {
                let s = unsafe { (*p).lock.lock() };
                for i in &s.lists {
                    crate::warn_on!(i.head.is_some());
                }
                crate::warn_on!(s.objs.iter().any(|o| o.nr != 0));
            }
            unsafe { ptr::drop_in_place(p) };
        }

        unsafe {
            free_percpu((*this).p.cast());
            (&raw mut (*this).p).write(ptr::null_mut());
            (*this).alloc.assume_init_drop();
        }
    }

    /// Everything queued processed, and freed: for an owner whose processing
    /// uses what goes before the RcuPending is dropped. A no-op on one never
    /// init()ed, or exit()ed - as the drop is then.
    ///
    /// # Safety
    /// Nothing enqueues or dequeues meanwhile, or after. @this is written
    /// through, so its provenance allows that: not a shared reference's, whose
    /// memory the compiler may take as unchanged - the drop then frees again.
    pub unsafe fn exit(this: *mut Self) {
        if unsafe { (*this).p }.is_null() {
            return;
        }
        unsafe { &*this }.drain();
        unsafe { Self::free_pcpu(this) }
    }
}

// Zeroed, it's one never init()ed: its percpu state null, A not there
unsafe impl<T: RcuRealloc, A: Alloc> Zeroable for RcuPending<'_, T, A> {}

impl<T: RcuRealloc, A: Alloc> Drop for RcuPending<'_, T, A> {
    fn drop(&mut self) {
        unsafe { Self::exit(self) }
    }
}

/// A T's memory, as alloc() gives it, for its user to write a T over - and
/// to drop before deallocate(): a build error for a T whose drop waits for
/// its grace period.
unsafe impl<T: RcuRealloc, A: Alloc> Allocator for RcuPending<'_, T, A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.allocate_from(layout, |a| a.allocate_zeroed(Layout::new::<T>()))
    }

    unsafe fn deallocate(&self, p: NonNull<u8>, _: Layout) {
        <T as RcuFreeing>::assert_drops_at_free();
        self.enqueue(p.cast())
    }
}

unsafe impl<T: RcuRealloc, A: Alloc> Alloc for RcuPending<'_, T, A> {}

/// A mempool's: its flags for what's fresh from A. One claimed is a T freed,
/// not zeroed: zeroed here if it's asked for.
unsafe impl<T: RcuRealloc, A: AllocGfp> AllocGfp for RcuPending<'_, T, A> {
    fn allocate_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
        let p = self.allocate_from(layout, |a| a.allocate_zeroed_gfp(Layout::new::<T>(), gfp))?;
        if gfp.contains(flags::ZERO) {
            unsafe { p.cast::<u8>().write_bytes(0, p.len()) };
        }
        Ok(p)
    }

    unsafe fn grow_gfp(&self, _: NonNull<u8>, _: Layout, _: Layout, _: Flags) -> Result<NonNull<[u8]>, AllocError> {
        Err(AllocError)
    }
}
