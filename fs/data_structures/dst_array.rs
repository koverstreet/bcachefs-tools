// SPDX-License-Identifier: GPL-2.0

//! DstArray<H, T, L>: a header, a count and the array after it in one
//! allocation, reached by a thin pointer to the header - the count says how
//! long the array is, as C's flexible array members are sized by a field
//! before them. For DstVec, and for util/rcu.rs's RcuArray.
//!
//! Allocated as RcuArray's always were, by kvzalloc() - so kvfree_rcu() can
//! free one - or by an allocator of its owner's; at least 8-aligned either
//! way: a thin pointer's low bits are its holder's to tag.
//!
//! Freed as it says (RcuFreeing): at once, or - what an RCU reader may still
//! be in, elements that aren't FreeAnytime - a grace period on. With no
//! rcu_head of its own, that's one before the header, where no reader looks:
//! six's wait list and multimap's vals free what they've grown out of under a
//! spinlock, where a kvfree_rcu() with no head to queue may sleep.

use crate::data_structures::array_vec::LenUint;
use crate::util::alloc::{flags, Alloc, AllocError, AllocGfp, Flags, KVmalloc};
use core::alloc::Layout;
use crate::util::maybe_rcu_alloc::RcuFreeing;
use crate::util::rcu::HasRcuHead;
use crate::util::thin::ThinBox;
use crate::util::vstructs::{align_max, DstHead, FlexArray, SelfSized, VStructError};
use core::ptr::{self, NonNull};

/// The count is its own, after the header: written only as it's made, so
/// nothing holding one can change how long it says the array is. 8-aligned,
/// as kvzalloc() aligns, so its size is what it's allocated as.
#[repr(C, align(8))]
pub struct DstArray<H, T, L: LenUint = usize> {
    head: H,
    nr: L,
    pub elems: [T],
}

impl<H, T, L: LenUint> DstArray<H, T, L> {
    /// The count's offset.
    const NR: usize = size_of::<H>().next_multiple_of(align_of::<L>());

    /// The array's offset.
    pub const TAIL: usize = (Self::NR + size_of::<L>()).next_multiple_of(align_of::<T>());

    pub fn head(&self) -> &H {
        &self.head
    }
}

// SAFETY: its count says how long the array is, and is written only as it's
// made
unsafe impl<H, T, L: LenUint> SelfSized for DstArray<H, T, L> {
    const ALIGN: usize = align_max(8, align_max(align_max(align_of::<H>(), align_of::<L>()), align_of::<T>()));
    const MIN_BYTES: usize = Self::TAIL.next_multiple_of(Self::ALIGN);

    unsafe fn from_thin(p: *const ()) -> *const Self {
        let nr = unsafe { *p.byte_add(Self::NR).cast::<L>() }.to_usize();
        // a slice pointer's length is the DST's tail's
        ptr::slice_from_raw_parts(p.cast::<T>(), nr) as *const Self
    }

    fn len_in(b: &[u8]) -> Result<usize, VStructError> {
        let ty = core::any::type_name::<Self>();
        if b.as_ptr() as usize % Self::ALIGN != 0 {
            return Err(VStructError::Misaligned { ty, align: Self::ALIGN });
        }
        let nr = b.get(Self::NR..).and_then(L::read)
            .ok_or(VStructError::Overrun { ty, want: Self::MIN_BYTES, have: b.len() })?;
        let len = nr.to_usize().checked_mul(size_of::<T>()).and_then(|n| n.checked_add(Self::TAIL))
            .and_then(|n| n.checked_next_multiple_of(Self::ALIGN)).unwrap_or(usize::MAX);
        if len > b.len() {
            return Err(VStructError::Overrun { ty, want: len, have: b.len() });
        }
        Ok(len)
    }
}

impl<H, T, L: LenUint> DstHead for DstArray<H, T, L> {
    type Head = H;
}

impl<H, T, L: LenUint> FlexArray for DstArray<H, T, L> {
    type Elem = T;

    fn nr(&self) -> usize {
        self.nr.to_usize()
    }
}

// its head is first
impl<H: HasRcuHead, T, L: LenUint> HasRcuHead for DstArray<H, T, L> {
    const HEAD: usize = H::HEAD;
    const WORK: bool = H::WORK;
}

impl<H, T, L: LenUint> DstArray<H, T, L> {
    /// What one of @nr elements is allocated as - its header, count and the
    /// array, after an rcu_head if it needs one it hasn't - and the header's
    /// offset. An error past what its count holds.
    pub fn layout(nr: usize) -> Result<(Layout, usize), AllocError> {
        const { assert!(size_of::<T>() != 0, "a DstArray of a zero sized type") };
        const { assert!(Self::ALIGN <= 8, "kvzalloc() aligns to 8") };

        if nr > L::MAX {
            return Err(AllocError);
        }
        let bytes = nr.checked_mul(size_of::<T>()).and_then(|b| b.checked_add(Self::TAIL)).ok_or(AllocError)?;
        // as Layout::for_value() has it, which its free goes by
        let value = Layout::from_size_align(bytes, Self::ALIGN).map_err(|_| AllocError)?.pad_to_align();
        <Self as RcuFreeing>::layout_of(value)
    }

    /// @head, and a count of @nr, at @offset in @given.
    fn place(head: H, nr: usize, given: NonNull<[u8]>, offset: usize) -> NonNull<H> {
        let p = unsafe { given.cast::<u8>().add(offset) };
        unsafe {
            p.cast::<H>().write(head);
            p.add(Self::NR).cast::<L>().write(L::from_usize(nr));
        }
        p.cast()
    }

    /// One of @nr elements, all zero, with @head.
    pub fn alloc(nr: usize, head: H) -> Result<NonNull<H>, AllocError> {
        Self::alloc_gfp(nr, head, Flags::empty())
    }

    /// alloc(), for what mustn't fail to allocate: a size past isize::MAX
    /// bytes, or past what its count holds, is a bug.
    pub fn alloc_nofail(nr: usize, head: H) -> NonNull<H> {
        Self::alloc_gfp(nr, head, flags::NOFAIL).expect("a DstArray past isize::MAX bytes, or its count")
    }

    fn alloc_gfp(nr: usize, head: H, gfp: Flags) -> Result<NonNull<H>, AllocError> {
        let (layout, offset) = Self::layout(nr)?;
        let given = KVmalloc.allocate_zeroed_gfp(layout, gfp)?;
        Ok(Self::place(head, nr, given, offset))
    }

    /// alloc(), owned: freed as its markers say.
    pub fn alloc_box(nr: usize, head: H) -> Result<ThinBox<Self, KVmalloc>, AllocError> {
        Self::alloc_box_gfp(nr, head, Flags::empty())
    }

    /// alloc_box(), for what mustn't fail to allocate.
    pub fn alloc_box_nofail(nr: usize, head: H) -> ThinBox<Self, KVmalloc> {
        Self::alloc_box_gfp(nr, head, flags::NOFAIL).expect("a DstArray past isize::MAX bytes, or its count")
    }

    fn alloc_box_gfp(nr: usize, head: H, gfp: Flags) -> Result<ThinBox<Self, KVmalloc>, AllocError> {
        let p = Self::alloc_gfp(nr, head, gfp)?;
        // KVmalloc's, ours, at layout()
        Ok(unsafe { ThinBox::from_raw(p.cast()) })
    }

    /// alloc(), of room for @nr from @a, its count the room @a gave - which
    /// may be more, and is what it's freed as: a mempool tells its elements by
    /// their size.
    pub fn alloc_in<A: Alloc>(nr: usize, head: H, a: &A) -> Result<NonNull<H>, AllocError> {
        let (layout, offset) = Self::layout(nr)?;
        let given = a.allocate_zeroed(layout)?;
        let room = (given.len() - offset - Self::TAIL) / size_of::<T>();
        Ok(Self::place(head, room.min(L::MAX), given, offset))
    }

    /// One of room for @nr from @a, as alloc_in(), @old's first @keep elements
    /// moved into it, the rest zero. @old stays allocated: freeing it is its
    /// owner's, now or after a grace period.
    ///
    /// # Safety
    /// @old is valid, and @keep within both. Its first @keep are moved, not
    /// copied: they're not to be dropped in @old.
    pub unsafe fn realloc_in<A: Alloc>(old: NonNull<H>, keep: usize, nr: usize, head: H, a: &A)
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

    /// Dropped and freed, now or a grace period on, as its markers say.
    ///
    /// # Safety
    /// @p is @a's - alloc()'s are KVmalloc's - its nr() the room @a gave, and
    /// nothing but RCU readers uses it after.
    pub unsafe fn free_in<A: Alloc>(p: NonNull<H>, a: &A) {
        let p = unsafe { NonNull::new_unchecked(Self::from_thin(p.as_ptr().cast()).cast_mut()) };
        unsafe { <Self as RcuFreeing>::free(p, a) };
    }
}
