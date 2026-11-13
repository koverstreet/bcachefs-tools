// SPDX-License-Identifier: GPL-2.0

//! A mempool, in both builds: elements of a size, from @A, with a reserve of
//! them set aside when it's made, so allocating can wait for one to be freed
//! where the allocator would fail.
//!
//! A pool of an allocator's elements is an allocator itself, of any size up
//! to its elements': Alloc, below.
//!
//! Zeroed, it isn't made; made after its owner, it's a Late<Mempool>, whose
//! lock serializes making it. It's exited when it drops, a no-op on one never
//! made. Allocating is GFP_KERNEL, as the task's memalloc scope says: it waits
//! for an element - or, in a NORECLAIM scope, where it can't, fails.

use crate::c;
use crate::errcode::BchError;
use crate::locking::Locked;
use crate::new_lock;
use crate::os::kernel::HZ;
use crate::util::alloc::{current_gfp_context_raw, kv_layout, Alloc, AllocError, KVec, KVmalloc, Kmalloc};
use crate::util::futex::EventCount;
use crate::util::late::LateInit;
use crate::util::projections::Volatile;
use crate::util::sleep::Sleep;
use core::alloc::Layout;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{fence, Ordering};
use pin_init::{pin_data, pinned_drop, PinInit};

#[pin_data(PinnedDrop)]
pub struct Mempool<A: Elements> {
    /* the reserve, and its length published, for free_raw()'s unlocked check */
    #[pin]
    reserve: Locked<c::spinlock_t, KVec<Element>, Volatile<usize>>,
    /* bumped by every element freed to the reserve, for alloc_raw() to wait on */
    freed:   EventCount,
    min_nr:  usize,
    size:    usize,
    _a:      PhantomData<fn() -> A>,
}

/// An element in the reserve: the pool's, for any thread to take.
struct Element(NonNull<u8>);

unsafe impl Send for Element {}

// SAFETY: zeroed is not made
unsafe impl<A: Elements> ::pin_init::Zeroable for Mempool<A> {}

unsafe impl<A: Elements> LateInit for Mempool<A> {
    fn is_made(&self) -> bool {
        self.size != 0
    }
}

/// Where a pool's elements come from, and go back to: C's mempool_alloc_t and
/// mempool_free_t.
///
/// # Safety
/// element_alloc() gives @layout's bytes - zeroed, with __GFP_ZERO - which
/// element_free() takes back.
pub unsafe trait Elements {
    fn element_alloc(layout: Layout, gfp: c::gfp_t) -> Option<NonNull<u8>>;

    /// # Safety
    /// @p is element_alloc()'s, of @layout, and not used after.
    unsafe fn element_free(p: NonNull<u8>, layout: Layout);
}

macro_rules! elements {
    ($($a:ident => $realloc:ident);*) => {$(
        unsafe impl Elements for $a {
            fn element_alloc(layout: Layout, gfp: c::gfp_t) -> Option<NonNull<u8>> {
                #[cfg(kernel)]
                {
                    use kernel::bindings::{$realloc, NUMA_NO_NODE};

                    NonNull::new(unsafe {
                        $realloc(core::ptr::null(), layout.size(), layout.align() as _, gfp, NUMA_NO_NODE)
                    }.cast())
                }
                #[cfg(not(kernel))]
                {
                    let p = if gfp & c::__GFP_ZERO != 0 { $a.alloc_zeroed(layout) } else { $a.alloc(layout) };
                    p.ok().map(NonNull::cast)
                }
            }

            unsafe fn element_free(p: NonNull<u8>, layout: Layout) {
                unsafe { $a.free(p, layout) }
            }
        }
    )*};
}

elements!(Kmalloc  => krealloc_node_align;
          KVmalloc => kvrealloc_node_align);

/// What's asked of the allocator for an element, as the task's memalloc scope
/// says: the pool is the reserve, so no emergency reserves, no looping in the
/// page allocator, and failing is fine.
fn element_gfp(gfp: c::gfp_t) -> c::gfp_t {
    current_gfp_context_raw(gfp) | c::__GFP_NOMEMALLOC | c::__GFP_NORETRY | c::__GFP_NOWARN
}

/// ...and before the reserve, no reclaim or IO either.
fn first_try(gfp: c::gfp_t) -> c::gfp_t {
    gfp & !(c::__GFP_DIRECT_RECLAIM | c::__GFP_IO)
}

/// The reserve: @min_nr elements, or none, if one can't be had.
fn fill<A: Elements>(min_nr: usize, layout: Layout) -> Result<KVec<Element>, AllocError> {
    let mut reserve = KVec::with_capacity(min_nr)?;
    let gfp = current_gfp_context_raw(c::GFP_KERNEL);

    for _ in 0..min_nr {
        let Some(p) = A::element_alloc(layout, gfp) else {
            drain::<A>(&mut reserve, layout);
            return Err(AllocError);
        };
        let _ = reserve.push_within_capacity(Element(p));
    }
    Ok(reserve)
}

fn drain<A: Elements>(reserve: &mut KVec<Element>, layout: Layout) {
    for e in reserve.drain_all() {
        unsafe { A::element_free(e.0, layout) };
    }
}

impl<A: Elements> Mempool<A> {
    /// @min_nr elements of @size bytes, A's. Nothing to undo if it fails.
    pub fn new(min_nr: usize, size: usize) -> impl PinInit<Self, BchError> {
        let layout = kv_layout(size);
        ::pin_init::pin_init!(Self {
            reserve <- new_lock!(fill::<A>(min_nr, layout?)?, Volatile::new(min_nr), "mempool"),
            freed: EventCount::new(),
            min_nr,
            size,
            _a: PhantomData,
        }? BchError)
    }

    /// The size of its elements: the most it allocates.
    pub fn element_size(&self) -> usize {
        self.size
    }

    fn layout(&self) -> Layout {
        kv_layout(self.size).expect("a made pool's element layout")
    }

    /// An element, for C to hold: free_raw() takes it back.
    pub fn alloc_raw(&self) -> Result<NonNull<u8>, AllocError> {
        self.alloc_raw_gfp(c::GFP_KERNEL)
    }

    /// alloc_raw(), allocating with @gfp - as the task's memalloc scope
    /// says.
    pub fn alloc_raw_gfp(&self, gfp: c::gfp_t) -> Result<NonNull<u8>, AllocError> {
        let gfp = element_gfp(gfp);
        let layout = self.layout();
        let mut gfp_temp = first_try(gfp);

        loop {
            if let Some(p) = A::element_alloc(layout, gfp_temp) {
                return Ok(p);
            }

            let seen = self.freed.snapshot();
            {
                let mut reserve = self.reserve.lock();
                if let Some(e) = reserve.pop() {
                    let nr = reserve.len();
                    reserve.published().set(nr);
                    drop(reserve);
                    /* paired with the fence in free_raw() */
                    fence(Ordering::Release);
                    return Ok(e.0);
                }
            }

            /*
             * The first round asks without direct reclaim or IO: if that
             * failed and the reserve was empty, retry at once with what was
             * asked.
             */
            if gfp_temp != gfp {
                gfp_temp = gfp;
                continue;
            }

            /* Userspace can always wait */
            if cfg!(kernel) && gfp & c::__GFP_DIRECT_RECLAIM == 0 {
                return Err(AllocError);
            }

            /*
             * Wait for someone else to return an element to the reserve - or
             * for the allocator to have memory again, which nothing wakes us
             * for.
             */
            let _ = self.freed.wait(seen, Sleep::UNINTERRUPTIBLE.io().timeout(5 * HZ));
        }
    }

    /// # Safety
    /// @p is this pool's, not used after.
    pub unsafe fn free_raw(&self, p: NonNull<u8>) {
        /*
         * Paired with the fence in alloc_raw(): the length read below is
         * from after @p was allocated, even if @p was passed to this task
         * without going through barriers.
         */
        fence(Ordering::Acquire);

        /*
         * For correctness, we need a test which is guaranteed to trigger if
         * the reserve's length + #allocated == min_nr. Testing that it's
         * short without locking achieves that, and refilling as soon as
         * possible is desirable. Waiters happen only when it's empty, and
         * this guarantees there will be frees that refill it and wake them.
         */
        if (*self.reserve).get() < self.min_nr {
            let mut reserve = self.reserve.lock();
            if reserve.len() < self.min_nr {
                assert!(reserve.push_within_capacity(Element(p)).is_ok(), "a mempool reserve with no room for min_nr");
                let nr = reserve.len();
                reserve.published().set(nr);
                drop(reserve);
                self.freed.wake();
                return;
            }
        }

        unsafe { A::element_free(p, self.layout()) }
    }
}

impl Mempool<Kmalloc> {
    /// mempool_init_kmalloc_pool().
    pub fn kmalloc(min_nr: usize, size: usize) -> impl PinInit<Self, BchError> {
        Self::new(min_nr, size)
    }
}

impl Mempool<KVmalloc> {
    pub fn kvmalloc(min_nr: usize, size: usize) -> impl PinInit<Self, BchError> {
        Self::new(min_nr, size)
    }
}

/// A pool as an allocator, of up to its elements' size: first what's asked,
/// if it's less than an element, from A cheaply, then an element, the worst
/// case. free() goes by the size alloc() gave: an element back to the pool,
/// anything smaller to A.
impl<A: Elements> Mempool<A> {
    /// Alloc's alloc(), allocating with @gfp - as the task's memalloc scope
    /// says.
    pub fn alloc_gfp(&self, layout: Layout, gfp: c::gfp_t) -> Result<NonNull<[u8]>, AllocError> {
        self.alloc_or_element(layout, false, gfp)
    }

    fn alloc_or_element(&self, layout: Layout, zeroed: bool, gfp: c::gfp_t) -> Result<NonNull<[u8]>, AllocError> {
        let size = self.element_size();
        if layout.size() > size || layout.align() > 8 {
            return Err(AllocError);
        }

        if layout.size() < size {
            let gfp = first_try(element_gfp(gfp));
            let gfp = if zeroed { gfp | c::__GFP_ZERO } else { gfp };
            if let Some(p) = A::element_alloc(layout, gfp) {
                return Ok(NonNull::slice_from_raw_parts(p, layout.size()));
            }
        }

        let p = self.alloc_raw_gfp(gfp)?;
        if zeroed {
            unsafe { p.write_bytes(0, size) };
        }
        Ok(NonNull::slice_from_raw_parts(p, size))
    }
}

unsafe impl<A: Elements> Alloc for Mempool<A> {
    fn alloc(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.alloc_or_element(layout, false, c::GFP_KERNEL)
    }

    fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.alloc_or_element(layout, true, c::GFP_KERNEL)
    }

    unsafe fn free(&self, p: NonNull<u8>, layout: Layout) {
        if layout.size() == self.element_size() {
            unsafe { self.free_raw(p) }
        } else {
            unsafe { A::element_free(p, layout) }
        }
    }
}

impl<A: Elements> Mempool<A> {
    /// A pool in a static - which never drops - made in place at module init.
    ///
    /// # Safety
    /// @this is unmade, zeroed, and nothing uses it till this returns.
    pub unsafe fn init_static(this: &'static Self, init: impl PinInit<Self, BchError>) -> Result<(), BchError> {
        unsafe { crate::util::alloc::init_at(init, core::ptr::from_ref(this).cast_mut()) }
    }

    /// The reserve freed, of a pool in a static, at module exit: a no-op on
    /// one never made.
    ///
    /// # Safety
    /// Nothing allocates from it, or frees to it, after.
    pub unsafe fn exit(&self) {
        if self.is_made() {
            let mut reserve = core::mem::replace(&mut *self.reserve.lock(), KVec::new());
            drain::<A>(&mut reserve, self.layout());
        }
    }
}

#[pinned_drop]
impl<A: Elements> PinnedDrop for Mempool<A> {
    fn drop(self: Pin<&mut Self>) {
        unsafe { self.exit() }
    }
}
