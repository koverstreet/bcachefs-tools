// SPDX-License-Identifier: GPL-2.0

//! A lockless queue, linked through a node in each item: llist.h's. push()
//! takes the item's owner, and the item is the list's until it's taken off -
//! nothing sees it meanwhile. Any number push and take at once. take() takes
//! everything, llist_del_all(): taking one, llist_del_first(), is open to ABA
//! against another taker. What's taken is reversed, to the order it was
//! pushed in, and the Taken owns it: what isn't consumed drops with it.
//!
//! The node is a field of the item, named: llist!(P, field).

use crate::util::place::Field;
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};

/// An item's link, while it's on an Llist.
#[repr(transparent)]
pub(crate) struct LlistNode(AtomicPtr<LlistNode>);

impl LlistNode {
    pub(crate) const fn new() -> Self {
        LlistNode(AtomicPtr::new(ptr::null_mut()))
    }
}

/// An owner of what goes on an Llist, given up while it's on it and taken
/// back as it's taken off: the kernel's ForeignOwnable.
///
/// # Safety
/// into_raw()'s Target is from_raw()'s to take back, once, and owned until
/// then: nothing else frees it, or links it.
pub(crate) unsafe trait LlistOwned {
    type Target;

    fn into_raw(self) -> NonNull<Self::Target>;

    /// # Safety
    /// @p is into_raw()'s, not yet taken back.
    unsafe fn from_raw(p: NonNull<Self::Target>) -> Self;
}

/// The Llist of @P, linked through its Target's @node field.
#[macro_export]
macro_rules! llist {
    ($p:ty, $node:ident) => {
        $crate::data_structures::llist::Llist<$p, { $crate::util::place::name(::core::stringify!($node)) }>
    };
}

pub(crate) struct Llist<P: LlistOwned, const N: u64>
where
    P::Target: Field<N, Type = LlistNode>,
{
    head: AtomicPtr<LlistNode>,
    _p:   PhantomData<P>,
}

// what's on it is moved between threads, never shared
unsafe impl<P: LlistOwned + Send, const N: u64> Sync for Llist<P, N> where P::Target: Field<N, Type = LlistNode> {}

impl<P: LlistOwned, const N: u64> Llist<P, N>
where
    P::Target: Field<N, Type = LlistNode>,
{
    pub(crate) const fn new() -> Self {
        Llist { head: AtomicPtr::new(ptr::null_mut()), _p: PhantomData }
    }

    /// Racy, as anything may push or take meanwhile: llist_empty().
    pub(crate) fn is_empty(&self) -> bool {
        self.head.load(Relaxed).is_null()
    }

    /// @p on it, the list's until it's taken: whether the list was empty -
    /// llist_add().
    pub(crate) fn push(&self, p: P) -> bool {
        let node: NonNull<LlistNode> = unsafe { p.into_raw().byte_add(<P::Target as Field<N>>::OFFSET) }.cast();
        let link = unsafe { node.as_ref() };
        let mut first = self.head.load(Relaxed);
        loop {
            link.0.store(first, Relaxed);
            match self.head.compare_exchange_weak(first, node.as_ptr(), Release, Relaxed) {
                Ok(_)    => return first.is_null(),
                Err(cur) => first = cur,
            }
        }
    }

    /// Everything on it, oldest first: llist_del_all(), then
    /// llist_reverse_order().
    pub(crate) fn take(&self) -> Taken<P, N> {
        let mut next = self.head.swap(ptr::null_mut(), Acquire);
        let mut oldest = ptr::null_mut();
        while let Some(node) = NonNull::new(next) {
            let n = unsafe { node.as_ref() };
            next = n.0.load(Relaxed);
            n.0.store(oldest, Relaxed);
            oldest = node.as_ptr();
        }
        Taken { next: oldest, _p: PhantomData }
    }
}

impl<P: LlistOwned, const N: u64> Drop for Llist<P, N>
where
    P::Target: Field<N, Type = LlistNode>,
{
    fn drop(&mut self) {
        drop(self.take())
    }
}

/// What take() took, oldest first.
pub(crate) struct Taken<P: LlistOwned, const N: u64>
where
    P::Target: Field<N, Type = LlistNode>,
{
    next: *mut LlistNode,
    _p:   PhantomData<P>,
}

impl<P: LlistOwned, const N: u64> Iterator for Taken<P, N>
where
    P::Target: Field<N, Type = LlistNode>,
{
    type Item = P;

    fn next(&mut self) -> Option<P> {
        let node = NonNull::new(self.next)?;
        // before it's handed back, and maybe freed
        self.next = unsafe { node.as_ref() }.0.load(Relaxed);
        Some(unsafe { P::from_raw(node.byte_sub(<P::Target as Field<N>>::OFFSET).cast()) })
    }
}

impl<P: LlistOwned, const N: u64> Drop for Taken<P, N>
where
    P::Target: Field<N, Type = LlistNode>,
{
    fn drop(&mut self) {
        self.for_each(drop)
    }
}
