// SPDX-License-Identifier: GPL-2.0

//! DstArray<H, T>: a header and the array after it in one allocation, reached
//! by a thin pointer to the header, which says how long the array is - as C's
//! flexible array members are sized by a field before them. For DstVec, and
//! for util/rcu.rs's RcuArray.
//!
//! Allocated as RcuArray's always were, by kvzalloc() - so kvfree_rcu() can
//! free one - and so at least 8-aligned: a thin pointer's low bits are its
//! holder's to tag.
//!
//! Freed as its elements say (util/rcu.rs): at once, or - what an RCU reader
//! may still be in, elements that aren't FreeAnytime - a grace period on, by
//! kvfree_rcu() of an rcu_head before the header.

use crate::c;
use crate::util::alloc::{flags, AllocError, AllocGfp, AllocMode, KVmalloc};
use core::alloc::Layout;
use crate::util::rcu::FreeAnytime;
use core::marker::PhantomData;
use core::ptr::{self, NonNull};

/// How an array of these is freed: prefix(), the bytes before the header -
/// the rcu_head - and free(), of the allocation's start.
trait ArrayFreeing {
    fn prefix() -> usize;
    unsafe fn free(base: *mut u8);
    fn assert_rcu_freed();
}

impl<T> ArrayFreeing for T {
    default fn prefix() -> usize { size_of::<c::rcu_head>().next_multiple_of(8) }
    default unsafe fn free(base: *mut u8) { unsafe { crate::util::alloc::kvfree_rcu_head(base.cast(), base.cast()) } }
    default fn assert_rcu_freed() {}
}

impl<T: FreeAnytime> ArrayFreeing for T {
    fn prefix() -> usize { 0 }
    unsafe fn free(base: *mut u8) { unsafe { crate::util::alloc::kvfree_unsized(base.cast()) } }
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

/// # Safety
/// nr() is the length of the array after it.
pub unsafe trait DstHead {
    fn nr(&self) -> usize;
}

#[repr(C)]
pub struct DstArray<H, T> {
    pub head: H,
    pub elems: [T],
}

impl<H, T> DstArray<H, T> {
    /// The array's offset.
    pub const TAIL: usize = size_of::<H>().next_multiple_of(align_of::<T>());
}

impl<H: DstHead, T> DstArray<H, T> {
    /// What a thin pointer to its header is to.
    ///
    /// # Safety
    /// @p points to a valid H, followed by its nr() Ts.
    pub unsafe fn from_thin(p: *mut H) -> *mut Self {
        // a slice pointer's length is the DST's tail's
        ptr::slice_from_raw_parts_mut(p.cast::<T>(), unsafe { (*p).nr() }) as *mut Self
    }

    /// One with @head, whose nr() it's sized by, and its elements all zero.
    /// An error past INT_MAX bytes, which kvmalloc() won't do.
    pub fn alloc(head: H) -> Result<NonNull<H>, AllocError> {
        Self::alloc_mode(head, AllocMode::Gfp(flags::GFP_KERNEL))
    }

    /// alloc(), for what mustn't fail to allocate: past INT_MAX bytes is a
    /// bug.
    pub fn alloc_nofail(head: H) -> NonNull<H> {
        Self::alloc_mode(head, AllocMode::Nofail).expect("a DstArray past INT_MAX bytes")
    }

    fn alloc_mode(head: H, mode: AllocMode) -> Result<NonNull<H>, AllocError> {
        const { assert!(size_of::<T>() != 0, "a DstArray of a zero sized type") };
        const { assert!(align_of::<H>() <= 8 && align_of::<T>() <= 8, "kvzalloc() aligns to 8") };

        let prefix = <T as ArrayFreeing>::prefix();
        let bytes = head.nr().checked_mul(size_of::<T>())
            .and_then(|b| b.checked_add(Self::TAIL + prefix))
            .filter(|&b| b <= i32::MAX as usize)
            .ok_or(AllocError)?;
        let layout = Layout::from_size_align(bytes, 8).map_err(|_| AllocError)?;

        let p = unsafe { KVmalloc.alloc_zeroed_mode(layout, mode)?.byte_add(prefix) }.cast::<H>();
        unsafe { p.write(head) };
        Ok(p)
    }

    /// One with @head, @old's first @keep elements moved into it, the rest
    /// zero. @old stays allocated: freeing it is its owner's, now or after a
    /// grace period.
    ///
    /// # Safety
    /// @old is valid, and @keep within both. Its first @keep are moved, not
    /// copied: they're not to be dropped in @old.
    pub unsafe fn realloc(old: NonNull<H>, keep: usize, head: H) -> Result<NonNull<H>, AllocError> {
        let new = Self::alloc(head)?;
        unsafe {
            let from = (*Self::from_thin(old.as_ptr())).elems.as_ptr();
            let to = (*Self::from_thin(new.as_ptr())).elems.as_mut_ptr();
            ptr::copy_nonoverlapping(from, to, keep);
        }
        Ok(new)
    }

    /// Now, or a grace period on: as its elements say.
    ///
    /// # Safety
    /// @p is alloc()'s, its elements dropped or moved, and nothing but RCU
    /// readers uses it after.
    pub unsafe fn free(p: NonNull<H>) {
        unsafe { <T as ArrayFreeing>::free(p.as_ptr().cast::<u8>().byte_sub(<T as ArrayFreeing>::prefix())) };
    }
}
