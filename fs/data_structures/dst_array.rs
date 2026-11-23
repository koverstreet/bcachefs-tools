// SPDX-License-Identifier: GPL-2.0

//! DstArray<H, T>: a header and the array after it in one allocation, reached
//! by a thin pointer to the header, which says how long the array is - as C's
//! flexible array members are sized by a field before them. For DstVec, and
//! for util/rcu.rs's RcuArray.
//!
//! Allocated as RcuArray's always were, by kvzalloc() - so kvfree_rcu() can
//! free one - or by an allocator of its owner's; at least 8-aligned either
//! way: a thin pointer's low bits are its holder's to tag.
//!
//! Freed as its elements say (util/rcu.rs): at once, or - what an RCU reader
//! may still be in, elements that aren't FreeAnytime - a grace period on, by
//! kvfree_rcu() of an rcu_head before the header, which only an allocator
//! kvfree() frees for can do.

use crate::c;
use crate::util::alloc::{flags, Alloc, AllocError, AllocGfp, Flags, KVmalloc};
use core::alloc::Layout;
use crate::util::rcu::FreeAnytime;
use crate::util::vstructs::{FlexArray, SelfSized};
use core::marker::PhantomData;
use core::ptr::{self, NonNull};

/// How an array of these is freed: prefix(), the bytes before the header -
/// the rcu_head - and free(), of the allocation's start.
trait ArrayFreeing {
    fn prefix() -> usize;
    unsafe fn free<A: Alloc>(base: NonNull<u8>, layout: Layout, a: &A);
    fn assert_rcu_freed();
}

impl<T> ArrayFreeing for T {
    default fn prefix() -> usize { size_of::<c::rcu_head>().next_multiple_of(8) }
    default unsafe fn free<A: Alloc>(base: NonNull<u8>, _: Layout, _: &A) {
        const { assert!(A::KVFREE, "an array RCU readers may be in, from an allocator kvfree_rcu() can't free to") };
        unsafe { crate::util::alloc::kvfree_rcu_head(base.as_ptr().cast(), base.as_ptr().cast()) }
    }
    default fn assert_rcu_freed() {}
}

impl<T: FreeAnytime> ArrayFreeing for T {
    fn prefix() -> usize { 0 }
    unsafe fn free<A: Alloc>(base: NonNull<u8>, layout: Layout, a: &A) { unsafe { a.deallocate(base, layout) } }
    fn assert_rcu_freed() { let () = NotRcuFreed::<T>::ERR; }
}

struct NotRcuFreed<T>(PhantomData<T>);

impl<T> NotRcuFreed<T> {
    const ERR: () = panic!("read under RCU, an array's elements are freed a grace period on: not FreeAnytime");
}

/// A build error unless an array of @T is freed a grace period on, as what
/// RCU readers read must be.
pub fn assert_rcu_freed<T>() {
    <T as ArrayFreeing>::assert_rcu_freed()
}

/// A DstArray's header: nr() is the length of the array after it.
///
/// # Safety
/// nr() reads nothing that changes through a &Self.
pub unsafe trait DstHead {
    fn nr(&self) -> usize;
}

/// Its header is had only by &: nothing holding a DstArray can change how
/// long it says the array is.
#[repr(C)]
pub struct DstArray<H, T> {
    head: H,
    pub elems: [T],
}

impl<H, T> DstArray<H, T> {
    /// The array's offset.
    pub const TAIL: usize = size_of::<H>().next_multiple_of(align_of::<T>());

    pub fn head(&self) -> &H {
        &self.head
    }
}

// SAFETY: the header says how long the array is - nr(), which nothing
// changes through it - and is had only by &
unsafe impl<H: DstHead, T> SelfSized for DstArray<H, T> {
    unsafe fn from_thin(p: *const ()) -> *const Self {
        // a slice pointer's length is the DST's tail's
        ptr::slice_from_raw_parts(p.cast::<T>(), unsafe { (*p.cast::<H>()).nr() }) as *const Self
    }
}

impl<H: DstHead, T> FlexArray for DstArray<H, T> {
    type Elem = T;

    fn nr(&self) -> usize {
        self.head.nr()
    }
}

impl<H: DstHead, T> DstArray<H, T> {
    /// What one of @nr elements is allocated as: its rcu_head, if it has one,
    /// its header, and the array.
    pub fn layout(nr: usize) -> Result<Layout, AllocError> {
        const { assert!(size_of::<T>() != 0, "a DstArray of a zero sized type") };
        const { assert!(align_of::<H>() <= 8 && align_of::<T>() <= 8, "kvzalloc() aligns to 8") };

        let bytes = nr.checked_mul(size_of::<T>())
            .and_then(|b| b.checked_add(Self::TAIL + <T as ArrayFreeing>::prefix()))
            .ok_or(AllocError)?;
        Layout::from_size_align(bytes, 8).map_err(|_| AllocError)
    }

    /// @head of the room @given has for elements, at the start of @given past
    /// its rcu_head.
    fn place(head: impl FnOnce(usize) -> H, given: NonNull<[u8]>) -> NonNull<H> {
        let prefix = <T as ArrayFreeing>::prefix();
        let room = (given.len() - prefix - Self::TAIL) / size_of::<T>();
        let p = unsafe { given.cast::<u8>().add(prefix) }.cast::<H>();
        unsafe { p.write(head(room)) };
        p
    }

    /// One with @head, whose nr() it's sized by, and its elements all zero.
    pub fn alloc(head: H) -> Result<NonNull<H>, AllocError> {
        Self::alloc_gfp(head, Flags::empty())
    }

    /// alloc(), for what mustn't fail to allocate: a size past isize::MAX
    /// bytes is a bug.
    pub fn alloc_nofail(head: H) -> NonNull<H> {
        Self::alloc_gfp(head, flags::NOFAIL).expect("a DstArray past isize::MAX bytes")
    }

    fn alloc_gfp(head: H, gfp: Flags) -> Result<NonNull<H>, AllocError> {
        let given = KVmalloc.allocate_zeroed_gfp(Self::layout(head.nr())?, gfp)?;
        Ok(Self::place(|_| head, given))
    }

    /// alloc(), of room for @nr from @a, its header @head of the room @a gave
    /// - which may be more, and is what it's freed as: a mempool tells its
    /// elements by their size.
    pub fn alloc_in<A: Alloc>(nr: usize, head: impl FnOnce(usize) -> H, a: &A) -> Result<NonNull<H>, AllocError> {
        let given = a.allocate_zeroed(Self::layout(nr)?)?;
        Ok(Self::place(head, given))
    }

    /// One of room for @nr from @a, as alloc_in(), @old's first @keep elements
    /// moved into it, the rest zero. @old stays allocated: freeing it is its
    /// owner's, now or after a grace period.
    ///
    /// # Safety
    /// @old is valid, and @keep within both. Its first @keep are moved, not
    /// copied: they're not to be dropped in @old.
    pub unsafe fn realloc_in<A: Alloc>(old: NonNull<H>, keep: usize, nr: usize, head: impl FnOnce(usize) -> H, a: &A)
        -> Result<NonNull<H>, AllocError>
    {
        let new = Self::alloc_in(nr, head, a)?;
        unsafe {
            let from = (*Self::from_thin(old.as_ptr().cast())).elems.as_ptr();
            let to = (*Self::from_thin(new.as_ptr().cast()).cast_mut()).elems.as_mut_ptr();
            ptr::copy_nonoverlapping(from, to, keep);
        }
        Ok(new)
    }

    /// Now, or a grace period on: as its elements say.
    ///
    /// # Safety
    /// @p is @a's - alloc()'s are KVmalloc's - its nr() the room @a gave, its
    /// elements dropped or moved, and nothing but RCU readers uses it after.
    pub unsafe fn free_in<A: Alloc>(p: NonNull<H>, a: &A) {
        let layout = Self::layout(unsafe { p.as_ref() }.nr()).expect("a DstArray's own layout");
        let base = unsafe { p.cast::<u8>().byte_sub(<T as ArrayFreeing>::prefix()) };
        unsafe { <T as ArrayFreeing>::free(base, layout, a) };
    }
}
