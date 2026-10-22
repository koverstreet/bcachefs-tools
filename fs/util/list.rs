// SPDX-License-Identifier: GPL-2.0

//! **Don't use this.** An intrusive list can't be made safe: linking an item
//! is a promise about its lifetime that nothing checks, and every list that
//! goes wrong goes wrong as use-after-free. New code wants a vector - KVVec,
//! DArray - and the lists we have are to become vectors. This exists for
//! what can't be one: the futex's waiters, whose entries live on their
//! stacks so that waiting can't fail to allocate, and C's lists while C has
//! them. Anything new needs an extremely good reason, said where it's used.
//!
//! include/linux/list.h's circular doubly linked list, in Rust, typed:
//! List<T, OFFSET> is a list of T linked through the links at OFFSET in each,
//! made by list!(T, member), which checks the member is links - ListLinks, or
//! C's struct list_head. Laid out as C's, so C's lists are these too,
//! from_c().
//!
//! What's safe, and why:
//!
//! - Holding the list is holding a &List or &mut List: one lives in a
//!   Lock<R, List<T, OFFSET>>, and from_c() asserts C's is locked. Walking it
//!   with a cursor, and reordering it - sort() - are safe.
//! - An item is live, and doesn't move, while it's on the list, and after
//!   it's taken off until the list's lock is next released: its owner takes
//!   the lock before freeing it. So a cursor can take an item off and still
//!   hand it back. Whoever links an item promises that: push_back() is
//!   unsafe, and so is remove(), which needs the item, if it's on a list, to
//!   be on this one.
//!
//! The links are Cells, written through shared references under the list's
//! lock; ListLinks, what an item embeds, is Sync on that ground.

use crate::c;
use core::cell::Cell;
use core::cmp::Ordering;
use core::marker::PhantomData;
use core::ptr;

/// struct list_head.
#[repr(C)]
struct ListHead {
    next: Cell<*const ListHead>,
    prev: Cell<*const ListHead>,
}

const _: () = assert!(size_of::<ListHead>() == size_of::<c::list_head>() &&
                      align_of::<ListHead>() == align_of::<c::list_head>());

impl ListHead {
    const fn new() -> Self {
        ListHead { next: Cell::new(ptr::null()), prev: Cell::new(ptr::null()) }
    }

    /// INIT_LIST_HEAD().
    fn init(&self) {
        self.next.set(self);
        self.prev.set(self);
    }

    /// No entries - or never init()ed, which is none either.
    fn is_empty(&self) -> bool {
        let next = self.next.get();
        next.is_null() || ptr::eq(next, self)
    }

    /// list_add_tail(): @new before this one.
    ///
    /// # Safety
    /// The list is held; @new is on none.
    unsafe fn add_tail(&self, new: &ListHead) {
        let prev = self.prev.get();
        new.next.set(self);
        new.prev.set(prev);
        unsafe { (*prev).next.set(new) };
        self.prev.set(new);
    }

    /// list_del_init().
    ///
    /// # Safety
    /// The list is held, and it's on it.
    unsafe fn del_init(&self) {
        let (prev, next) = (self.prev.get(), self.next.get());
        unsafe {
            (*next).prev.set(prev);
            (*prev).next.set(next);
        }
        self.init();
    }
}

/// What a list's item embeds, to be on it.
#[repr(transparent)]
pub struct ListLinks(ListHead);

// SAFETY: touched only through its list, under the list's lock
unsafe impl Send for ListLinks {}
unsafe impl Sync for ListLinks {}

impl ListLinks {
    pub const fn new() -> Self {
        ListLinks(ListHead::new())
    }
}

/// What list!() accepts as a member: links, Rust's or C's.
pub trait IsLinks {}
impl IsLinks for ListLinks {}
impl IsLinks for c::list_head {}

/// A list of T, linked through the links at OFFSET in each: list!().
#[repr(transparent)]
pub struct List<T, const OFFSET: usize> {
    head: ListHead,
    _t:   PhantomData<*const T>,
}

/// The List of @T through its member @member, which must be links.
#[macro_export]
macro_rules! list {
    ($T:ty, $member:ident) => {
        $crate::util::list::List<$T, {
            fn _member_is_links(t: &$T) -> &impl $crate::util::list::IsLinks { &t.$member }
            ::core::mem::offset_of!($T, $member)
        }>
    };
}

// SAFETY: the thread holding it is handed &T - T: Sync
unsafe impl<T: Sync, const OFFSET: usize> Send for List<T, OFFSET> {}

impl<T, const OFFSET: usize> List<T, OFFSET> {
    const FITS: () = assert!(OFFSET + size_of::<ListHead>() <= size_of::<T>(),
                             "List: links past the end of their item");

    /// INIT_LIST_HEAD(): empty - from zeroed, which isn't a list yet.
    ///
    /// # Safety
    /// It doesn't move while it has anything on it.
    pub unsafe fn init(&self) {
        self.head.init()
    }

    /// C's, held.
    ///
    /// # Safety
    /// @p is a live list of T, through the list_head at OFFSET in each, whose
    /// lock is held for 'a, and which nothing else in Rust borrows meanwhile.
    pub unsafe fn from_c<'a>(p: *mut c::list_head) -> &'a mut Self {
        let () = Self::FITS;
        unsafe { &mut *p.cast() }
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_empty()
    }

    fn links(item: &T) -> &ListHead {
        let () = Self::FITS;
        // SAFETY: list!()'s: the links at OFFSET in a T
        unsafe { &*ptr::from_ref(item).byte_add(OFFSET).cast() }
    }

    /// # Safety
    /// @l is on this list, or was taken off it with the list held since.
    unsafe fn item<'a>(l: *const ListHead) -> &'a T {
        unsafe { &*l.byte_sub(OFFSET).cast() }
    }

    /// list_add_tail().
    ///
    /// # Safety
    /// The list is init()ed. @item is on no list, and is live and doesn't move
    /// until it's off this one and the list's lock has been released since:
    /// its owner takes the lock before freeing it.
    pub unsafe fn push_back(&mut self, item: &T) {
        unsafe { self.head.add_tail(Self::links(item)) }
    }

    /// Takes @item off, if it's on: whether it was.
    ///
    /// # Safety
    /// If it's on a list, it's this one.
    pub unsafe fn remove(&mut self, item: &T) -> bool {
        let l = Self::links(item);
        let on = !l.is_empty();
        if on {
            unsafe { l.del_init() };
        }
        on
    }

    /// From the front: a place in the list, which can take its item off.
    pub fn cursor_mut(&mut self) -> CursorMut<'_, T, OFFSET> {
        let pos = if self.is_empty() { ptr::from_ref(&self.head) } else { self.head.next.get() };
        CursorMut { list: self, pos }
    }

    /// Its items in @cmp's order, stably: an insertion sort, from the back,
    /// for a list kept nearly in order - one in order costs a pass, and each
    /// item out of order the distance it moves.
    pub fn sort(&mut self, mut cmp: impl FnMut(&T, &T) -> Ordering) {
        if self.is_empty() {
            return;
        }
        let head: *const ListHead = &self.head;
        // SAFETY: everything followed is on the list, which we hold
        unsafe {
            let mut cmp = |a, b| cmp(Self::item(a), Self::item(b));

            // the first of the sorted run at the back
            let mut pos = self.head.prev.get();
            loop {
                let e = (*pos).prev.get();
                if ptr::eq(e, head) {
                    break;
                }
                if cmp(e, pos) != Ordering::Greater {
                    pos = e;
                    continue;
                }
                // before the first in the run it isn't above - any equal to
                // it came after it, and still do
                (*e).del_init();
                let mut q = (*pos).next.get();
                while !ptr::eq(q, head) && cmp(q, e) == Ordering::Less {
                    q = (*q).next.get();
                }
                (*q).add_tail(&*e);
            }
        }
    }
}

/// A place in a held list.
pub struct CursorMut<'a, T, const OFFSET: usize> {
    list: &'a mut List<T, OFFSET>,
    pos:  *const ListHead,
}

impl<'a, T, const OFFSET: usize> CursorMut<'a, T, OFFSET> {
    fn at_end(&self) -> bool {
        ptr::eq(self.pos, &self.list.head)
    }

    pub fn current(&self) -> Option<&'a T> {
        // SAFETY: on the list, which we hold for 'a
        (!self.at_end()).then(|| unsafe { List::<T, OFFSET>::item(self.pos) })
    }

    pub fn move_next(&mut self) {
        if !self.at_end() {
            // SAFETY: on the list
            self.pos = unsafe { (*self.pos).next.get() };
        }
    }

    /// Takes the current item off, onto the next: still live for 'a, the
    /// list's held for that long.
    pub fn remove_current(&mut self) -> Option<&'a T> {
        let item = self.current()?;
        let cur = self.pos;
        // SAFETY: on the list, which we hold
        unsafe {
            self.pos = (*cur).next.get();
            (*cur).del_init();
        }
        Some(item)
    }
}
