// SPDX-License-Identifier: GPL-2.0

//! Objects waiting out an RCU or SRCU grace period, then processed - per CPU,
//! batched by the grace period they wait for - and taken back before then if
//! wanted. What processing is, and what's queued, is the Mode: the owner's
//! process callback, or kvfree_rcu().

use core::ffi::c_uint;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{offset_of, ManuallyDrop};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use pin_init::{pin_init_from_closure, PinInit, Zeroable};

use crate::c;
use crate::locking::Locked;
use crate::new_lock;
use crate::types::GenRadix;
use crate::util::alloc::{flags, init_at, kbox_zeroed_nofail, AllocError, AllocMode, KBox, KVBox, KVVec};
use crate::util::ffi::Opaque;
use crate::data_structures::genradix::{NewNode, NODE_SIZE};
use crate::util::alloc::{KVmalloc, Kmalloc};
use crate::util::maybe_rcu_alloc::{alloc_at, Retired};
use crate::os::percpu::{free_percpu, CpuShared, PerCpu, PerCpuRef};
use crate::util::rcu::{Flavor, GpCookie};
use crate::os::workqueue::{flush_work, Queue};

const ACTIVE: usize = crate::os::rcu::NUM_ACTIVE_POLL_OLDSTATE;

/// An item, given up: its rcu_head, if it has one - what the lists thread
/// through - or else its pointer.
type Raw = NonNull<u8>;

/// What an RcuPending does with what it queues once readers are past it -
/// and what it queues.
///
/// # Safety
/// from_raw() takes back into_raw()'s - for HAS_HEAD, the item's rcu_head,
/// unused but by this while it's queued.
pub unsafe trait Mode: Sized {
    type Item;

    /// Whether an item has an rcu_head of its own. One that hasn't is given
    /// one, allocated, if it has to go on a list: kvfree_rcu_mightsleep()'s.
    const HAS_HEAD: bool = true;

    fn into_raw(item: Self::Item) -> Raw;

    /// # Safety
    /// @p is into_raw()'s, taken back once.
    unsafe fn from_raw(p: Raw) -> Self::Item;

    /// # Safety
    /// @item's grace period is over.
    unsafe fn process(&self, _pending: &RcuPending<'_, Self>, item: Self::Item) {
        drop(item)
    }
}

/// kvfree_rcu(), batched: a MaybeRcuAlloc's memory, its T dropped when it was
/// retired, freed a grace period after it's queued.
pub struct KvFree<T>(PhantomData<fn(T)>);

unsafe impl<T> Mode for KvFree<T> {
    type Item = Retired<T, KVmalloc>;

    fn into_raw(item: Retired<T, KVmalloc>) -> Raw {
        item.into_head().cast()
    }

    unsafe fn from_raw(p: Raw) -> Retired<T, KVmalloc> {
        unsafe { Retired::from_head(p.cast()) }
    }

    unsafe fn process(&self, _pending: &RcuPending<'_, Self>, item: Retired<T, KVmalloc>) {
        unsafe { item.free() }
    }
}

/// kvfree_rcu_mightsleep(), batched: KvFree, of a T without an rcu_head.
pub struct KvFreeMightSleep<T>(PhantomData<fn(T)>);

unsafe impl<T: Send> Mode for KvFreeMightSleep<T> {
    type Item = KVBox<T>;
    const HAS_HEAD: bool = false;

    fn into_raw(item: KVBox<T>) -> Raw {
        NonNull::new(KVBox::into_raw(item)).expect("a box's pointer").cast()
    }

    unsafe fn from_raw(p: Raw) -> KVBox<T> {
        unsafe { KVBox::from_raw(p.as_ptr().cast()) }
    }
}

/// The head a list holds an item without one by, in a box of its own.
#[repr(C)]
struct SpareHead {
    head: c::rcu_head,
    raw:  Option<Raw>,
}

unsafe impl Zeroable for SpareHead {}

impl SpareHead {
    fn alloc_atomic() -> Option<KBox<SpareHead>> {
        let init = unsafe {
            pin_init_from_closure(|slot: *mut SpareHead| {
                slot.write(Zeroable::zeroed());
                Ok::<(), AllocError>(())
            })
        };
        let p = alloc_at::<SpareHead, Kmalloc, AllocError>(init, AllocMode::Gfp(flags::__GFP_NOWARN)).ok()?;
        Some(unsafe { KBox::from_raw(p.as_ptr()) })
    }
}

/// @h's link: the kernel's next, or liburcu's next.next.
fn link(h: NonNull<c::rcu_head>) -> *mut Option<NonNull<c::rcu_head>> {
    #[cfg(kernel)]
    return unsafe { (&raw mut (*h.as_ptr()).next).cast() };
    #[cfg(not(kernel))]
    return unsafe { (&raw mut (*h.as_ptr()).next.next).cast() };
}

/// A list of queued items, through their heads - the list's while they're on
/// it.
#[derive(Default)]
struct List {
    head: Option<NonNull<c::rcu_head>>,
    tail: Option<NonNull<c::rcu_head>>,
    seq:  GpCookie,
}

impl List {
    fn add(&mut self, n: NonNull<c::rcu_head>) {
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
    fn remove_where(&mut self, mut f: impl FnMut(NonNull<c::rcu_head>) -> bool) -> Option<NonNull<c::rcu_head>> {
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

struct Drain(Option<NonNull<c::rcu_head>>);

impl Iterator for Drain {
    type Item = NonNull<c::rcu_head>;

    fn next(&mut self) -> Option<NonNull<c::rcu_head>> {
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
    /// Where the next push goes, in a node allocated already
    cursor: *mut Option<Raw>,
    seq:    GpCookie,
}

impl Seq {
    fn new(seq: GpCookie) -> Self {
        Seq { objs: Zeroable::zeroed(), nr: 0, cursor: ptr::null_mut(), seq }
    }

    fn get(&self, i: usize) -> Raw {
        self.objs.get(i).copied().flatten().expect("a seq's entry, below its nr")
    }

    fn iter(&self) -> impl Iterator<Item = Raw> + '_ {
        (0..self.nr).map(|i| self.get(i))
    }

    /// Onto the end: false if that needs a node, and there's none to be had
    /// atomically - nor in @node.
    fn push(&mut self, raw: Raw, node: &mut NewNode) -> bool {
        if self.cursor.is_null() {
            match self.objs.get_or_alloc_preallocated(self.nr, node, c::GFP_ATOMIC | c::__GFP_NOWARN) {
                Ok(slot) => self.cursor = ptr::from_mut(slot),
                Err(_)   => return false,
            }
        }

        unsafe { self.cursor.write(Some(raw)) };
        self.cursor = self.cursor.wrapping_add(1);
        /* zero cursor if we hit the end of a radix tree node: */
        if self.cursor as usize & (NODE_SIZE - 1) == 0 {
            self.cursor = ptr::null_mut();
        }
        self.nr += 1;
        true
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
        self.cursor = ptr::null_mut();
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
    objs:     KVVec<Seq>,

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
    /// Items, not Seqs: one can be left empty, by dequeue_where(), or by an
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
        self.objs.push_gfp(Seq::new(seq), flags::GFP_ATOMIC).ok()?;
        self.objs.last_mut()
    }
}

#[repr(C)]
struct Pcpu<'f, M: Mode> {
    parent: *const RcuPending<'f, M>,
    lock:   Locked<c::spinlock_t, PcpuState>,
    /* what's queued here: added under the lock, taken off as it's processed */
    nr:     AtomicUsize,
    cpu:    c_uint,
    cb:     Opaque<c::rcu_head>,
    work:   Opaque<c::work_struct>,
}

// what other CPUs change is under the lock, or C's - cb and work
unsafe impl<M: Mode> CpuShared for Pcpu<'_, M> {}

/// Why enqueue_list() couldn't: what to do with the lock dropped, before
/// trying again.
enum ListBlocked {
    /// Allocate a spare head, which couldn't be had atomically
    NeedHead,
    /// Nothing: the item's grace period is over, and it's processed
    Expired,
}

/// Pinned: each CPU's state points back to it.
pub struct RcuPending<'f, M: Mode> {
    p:       *mut Pcpu<'f, M>,
    srcu:    Flavor<'f>,
    process: M,
    _pin:    PhantomData<PhantomPinned>,
}

unsafe impl<M: Mode + Send> Send for RcuPending<'_, M> {}
unsafe impl<M: Mode + Sync> Sync for RcuPending<'_, M> {}

impl<'f, M: Mode> RcuPending<'f, M> {
    /// With @srcu's grace periods, @process done with what's queued once its
    /// grace period has completed.
    pub fn new(srcu: Flavor<'f>, process: M) -> impl PinInit<Self, AllocError> {
        unsafe { pin_init_from_closure(move |slot: *mut Self| Self::init(slot, srcu, process)) }
    }

    fn pcpu(&self) -> PerCpuRef<'_, Pcpu<'f, M>> {
        unsafe { PerCpuRef::from_raw(self.p) }
    }

    fn process_raw(&self, raw: Raw) {
        unsafe { self.process.process(self, M::from_raw(raw)) }
    }

    /// A list entry's item: the head itself, or what a spare head holds.
    fn list_raw(h: NonNull<c::rcu_head>) -> Raw {
        if M::HAS_HEAD {
            h.cast()
        } else {
            unsafe { (*h.cast::<SpareHead>().as_ptr()).raw }.expect("a spare head's item")
        }
    }

    /// list_raw(), off the list for good: a spare head freed.
    fn list_take(h: NonNull<c::rcu_head>) -> Raw {
        if M::HAS_HEAD {
            return h.cast();
        }
        let spare = unsafe { KBox::from_raw(h.cast::<SpareHead>().as_ptr()) };
        spare.raw.expect("a spare head's item")
    }

    /// What take_expired() took out of @p, processed - with the lock dropped.
    fn process_expired(&self, p: &Pcpu<'f, M>, e: Expired) {
        for raw in e.objs.iter().flat_map(Seq::iter) {
            self.process_raw(raw);
            p.nr.fetch_sub(1, Relaxed);
        }
        drop(e.objs);

        for h in e.list {
            self.process_raw(Self::list_take(h));
            p.nr.fetch_sub(1, Relaxed);
        }
    }

    unsafe extern "C" fn work_fn(work: *mut c::work_struct) {
        let p = unsafe { &*work.byte_sub(offset_of!(Pcpu<'f, M>, work)).cast::<Pcpu<'f, M>>() };
        let pending = unsafe { &*p.parent };

        loop {
            let e = p.lock.lock().take_expired(pending.srcu);
            let Some(e) = e else { break };
            pending.process_expired(p, e);
        }
    }

    unsafe extern "C" fn rcu_cb(rcu: *mut c::rcu_head) {
        let p = unsafe { &*rcu.byte_sub(offset_of!(Pcpu<'f, M>, cb)).cast::<Pcpu<'f, M>>() };
        let pending = unsafe { &*p.parent };

        unsafe { Queue::system().queue_work_on(p.cpu, p.work.as_ptr()) };

        let mut s = p.lock.lock();
        if s.has_pending() {
            drop(s);
            unsafe { pending.srcu.call(p.cb.as_ptr(), Self::rcu_cb) };
        } else {
            s.cb_armed = false;
        }
    }

    /// Onto a list, the radix being out of reach: whether the list is new,
    /// and wants a grace period started.
    fn enqueue_list(&self, s: &mut PcpuState, seq: GpCookie, raw: Raw, spare: &mut Option<KBox<SpareHead>>)
        -> Result<bool, ListBlocked>
    {
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
                return Err(ListBlocked::Expired);
            }
        };

        let head = if M::HAS_HEAD {
            raw.cast()
        } else {
            /*
             * kvfree_rcu_mightsleep(): we weren't passed an
             * rcu_head, but we need one: allocate a head to hold the pointer
             * to free by
             */
            let mut h = spare.take().or_else(SpareHead::alloc_atomic).ok_or(ListBlocked::NeedHead)?;
            h.raw = Some(raw);
            NonNull::new(KBox::into_raw(h)).expect("a box's pointer").cast()
        };

        /*
         * Empty, it wants a grace period started - as an empty Seq does -
         * even with our seq: dequeue_where() may have emptied it, and the
         * callback have found nothing pending since and disarmed
         */
        let l = &mut s.lists[i];
        let new = l.head.is_none();
        l.seq = seq;
        l.add(head);
        Ok(new)
    }

    /// Queue @item, to be processed once its grace period has elapsed.
    ///
    /// Attempt to enqueue items onto a radix tree; if memory allocation fails,
    /// fall back to a linked list. Will do GFP_KERNEL memory allocations, and
    /// process expired items.
    pub fn enqueue(&self, item: M::Item) {
        let raw = M::into_raw(item);
        let mut may_sleep = true;
        let mut node = NewNode::none();
        let mut spare = None;

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
                        may_sleep = node.alloc(!M::HAS_HEAD).is_ok();
                        continue 'retry;
                    }
                }

                match self.enqueue_list(s, seq, raw, &mut spare) {
                    Ok(start) => start,
                    Err(b) => {
                        lock = None;
                        if let ListBlocked::NeedHead = b {
                            spare = Some(kbox_zeroed_nofail());
                        }
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
                    unsafe { self.srcu.call(p.cb.as_ptr(), Self::rcu_cb) };
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

    fn pcpu_dequeue(&self, p: &Pcpu<'f, M>, mut try_claim: impl FnMut(&M::Item) -> bool) -> Option<M::Item> {
        let mut claimed = |raw: Raw| try_claim(&ManuallyDrop::new(unsafe { M::from_raw(raw) }));
        let mut s = p.lock.lock();

        let raw = match s.objs.iter_mut().find_map(|o| o.remove_where(&mut claimed)) {
            Some(raw) => raw,
            None => {
                let h = s.lists.iter_mut().find_map(|l| l.remove_where(|h| claimed(Self::list_raw(h))))?;
                Self::list_take(h)
            }
        };
        p.nr.fetch_sub(1, Relaxed);
        Some(unsafe { M::from_raw(raw) })
    }

    /// The first entry @try_claim returns true for, dequeued: @try_claim
    /// is called with the CPU's lock held, and returns true if the caller has
    /// claimed the entry (e.g. taken a lock on it) and it should be dequeued.
    /// Must not sleep.
    ///
    /// Iterates the current CPU's pending lists in order (most recent radix-tree
    /// entries first, then oldstate lists). Returns the first claimed entry.
    /// Remaining entries stay queued.
    pub fn dequeue_where(&self, try_claim: impl FnMut(&M::Item) -> bool) -> Option<M::Item> {
        self.pcpu_dequeue(self.pcpu().raw_this_cpu(), try_claim)
    }

    /// dequeue_where(), from every CPU.
    pub fn dequeue_from_all_where(&self, mut try_claim: impl FnMut(&M::Item) -> bool) -> Option<M::Item> {
        if let Some(ret) = self.dequeue_where(&mut try_claim) {
            return Some(ret);
        }
        self.pcpu().each_cpu().find_map(|p| self.pcpu_dequeue(p, &mut try_claim))
    }

    /// How many items are queued, not yet processed or dequeued.
    pub fn nr_pending(&self) -> usize {
        self.pcpu().each_cpu().map(|p| p.nr.load(Relaxed)).sum()
    }

    fn has_pending_or_armed(&self) -> bool {
        self.pcpu().each_cpu().any(|p| {
            let s = p.lock.lock();
            s.has_pending() || s.cb_armed
        })
    }

    unsafe extern "C" fn init_pcpu(slot: *mut Pcpu<'f, M>, this: *mut Self, cpu: c_uint) {
        let lock = new_lock!(PcpuState {
            objs:     KVVec::new(),
            lists:    Default::default(),
            cb_armed: false,
        }, "rcu_pending_pcpu.lock");
        let work = crate::new_work!(Self::work_fn, "rcu_pending_pcpu.work");
        unsafe {
            (&raw mut (*slot).parent).write(this);
            (&raw mut (*slot).nr).write(AtomicUsize::new(0));
            (&raw mut (*slot).cpu).write(cpu);
            let Ok(()) = init_at(lock, &raw mut (*slot).lock);
            let Ok(()) = init_at(work, (&raw mut (*slot).work).cast());
        }
    }

    /// new(), at @this.
    ///
    /// # Safety
    /// @this doesn't move until it's freed.
    unsafe fn init(this: *mut Self, srcu: Flavor<'f>, process: M) -> Result<(), AllocError> {
        let p = unsafe { PerCpu::alloc_init(Self::init_pcpu, this) }?.into_raw();

        unsafe {
            (&raw mut (*this).p).write(p);
            (&raw mut (*this).srcu).write(srcu);
            (&raw mut (*this).process).write(process);
        }
        Ok(())
    }

    /// Everything queued waited for, and processed - with what processing's
    /// queued run.
    fn drain(&self) {
        while self.has_pending_or_armed() {
            self.srcu.barrier();

            for p in self.pcpu().each_cpu() {
                unsafe { flush_work(p.work.as_ptr()) };
            }
        }

        for p in self.pcpu().each_cpu() {
            unsafe { flush_work(p.work.as_ptr()) };
        }
    }

    /// # Safety
    /// drain()ed, and nothing enqueues after: exit()'s @this.
    unsafe fn free(this: *mut Self) {
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
        unsafe { Self::free(this) }
    }
}

// Zeroed, it's one never init()ed: its percpu state null
unsafe impl<M: Mode + Zeroable> Zeroable for RcuPending<'_, M> {}

impl<M: Mode> Drop for RcuPending<'_, M> {
    fn drop(&mut self) {
        unsafe { Self::exit(self) }
    }
}
