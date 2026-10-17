// SPDX-License-Identifier: GPL-2.0

//! GenRadix<T>, safely - linux/generic-radix-tree.h: a sparse array of T,
//! allocated a node at a time and zeroed, whose entries never move once they
//! exist.
//!
//! The split is C's, and is to be kept when genradix itself is Rust: what's
//! here is generic and #[inline], and turns an index into a byte offset with
//! size_of::<T>() a constant - so a size that's a power of two is a shift,
//! and one that isn't a multiply and shift for the divide and the modulo by
//! objects per node; and under it, the core - C's __genradix_*() - works in
//! byte offsets and isn't generic: one copy, not one per T.
//!
//! An entry in an allocated node is zeroed memory until something writes it,
//! and so is a T only if zeroed is one: T: Zeroable. Allocating takes &mut -
//! two callers allocating one index would otherwise each hold a &mut T - and
//! where C shares the radix, Rust's access is under the lock C's is.
//!
//! No Drop: GenRadix is Copy, as the C structs holding one are, and those free
//! theirs in C. free() is genradix_free().

use crate::c;
use crate::types::GenRadix;
use crate::util::alloc::AllocError;
use core::mem::size_of;
use core::ptr::NonNull;
use pin_init::Zeroable;

const NODE_SIZE: usize = c::GENRADIX_NODE_SIZE as usize;

impl<T: Zeroable> GenRadix<T> {
    /// What C's BUILD_BUG_ON() in __idx_to_offset() says. Nodes are
    /// NODE_SIZE-aligned - kmalloc() of a power of two, in either build - and
    /// entries at multiples of T's size from a node's start, so every entry is
    /// aligned too.
    const FITS: () = assert!(size_of::<T>() != 0 && size_of::<T>() <= NODE_SIZE,
                             "GenRadix: an entry bigger than a node, or zero-sized");

    /// @idx's byte offset - as __idx_to_offset(), None past usize. An entry
    /// doesn't straddle nodes: for a size that doesn't divide NODE_SIZE, a
    /// node's tail is unused.
    #[inline]
    fn offset(idx: usize) -> Option<usize> {
        let () = Self::FITS;
        let size = size_of::<T>();

        if size.is_power_of_two() {
            idx.checked_mul(size)
        } else {
            let per_node = NODE_SIZE / size;
            (idx / per_node).checked_mul(NODE_SIZE)?.checked_add((idx % per_node) * size)
        }
    }

    fn tree(&self) -> *mut c::__genradix {
        // C's signatures aren't const; lookups only read
        core::ptr::addr_of!(self.tree).cast_mut()
    }

    /// The entry at @idx - None if its node was never allocated: as
    /// genradix_ptr().
    #[inline]
    pub fn get(&self, idx: usize) -> Option<&T> {
        let p = unsafe { c::__genradix_ptr(self.tree(), Self::offset(idx)?) };
        // SAFETY: an entry of an allocated node - a T, by Zeroable, if
        // nothing has written it
        NonNull::new(p.cast::<T>()).map(|p| unsafe { p.as_ref() })
    }

    /// get(), to edit.
    #[inline]
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut T> {
        let p = unsafe { c::__genradix_ptr(&mut self.tree, Self::offset(idx)?) };
        // SAFETY: as get()'s, and &mut self
        NonNull::new(p.cast::<T>()).map(|mut p| unsafe { p.as_mut() })
    }

    /// The entry at @idx, allocating its node - zeroed - if it hasn't been:
    /// as genradix_ptr_alloc().
    #[inline]
    pub fn get_or_alloc(&mut self, idx: usize, gfp: c::gfp_t) -> Result<&mut T, AllocError> {
        let offset = Self::offset(idx).ok_or(AllocError)?;
        let p = unsafe { c::__genradix_ptr_alloc(&mut self.tree, offset, core::ptr::null_mut(), gfp) };
        // SAFETY: as get_mut()'s
        NonNull::new(p.cast::<T>()).map(|mut p| unsafe { p.as_mut() }).ok_or(AllocError)
    }

    /// The entries from @idx on, with their indices - every entry of each
    /// allocated node, written or not: as genradix_for_each_from().
    #[inline]
    pub fn iter_from(&self, idx: usize) -> Iter<'_, T> {
        // genradix_iter_init(): an offset past usize is the end
        let offset = Self::offset(idx).unwrap_or(usize::MAX);
        Iter { radix: self, iter: c::genradix_iter { offset, pos: idx } }
    }

    /// Free every node: as genradix_free(). Empty after, as a new one is.
    pub fn free(&mut self) {
        unsafe { c::__genradix_free(&mut self.tree) }
    }
}

pub struct Iter<'a, T> {
    radix: &'a GenRadix<T>,
    iter:  c::genradix_iter,
}

impl<'a, T: Zeroable> Iterator for Iter<'a, T> {
    type Item = (usize, &'a T);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let size = size_of::<T>();

        // genradix_iter_peek(): which skips unallocated nodes, moving the
        // iterator to the next allocated one
        let p = unsafe { c::__genradix_iter_peek(&mut self.iter, self.radix.tree(), NODE_SIZE / size) };
        // SAFETY: as GenRadix::get()'s
        let item = (self.iter.pos, unsafe { NonNull::new(p.cast::<T>())?.as_ref() });

        // genradix_iter_advance(): past the node's last whole entry, to the
        // next node
        let next = self.iter.offset.checked_add(size).and_then(|offset| {
            if !size.is_power_of_two() && (offset & (NODE_SIZE - 1)) + size > NODE_SIZE {
                offset.checked_next_multiple_of(NODE_SIZE)
            } else {
                Some(offset)
            }
        });
        match next {
            Some(offset) => {
                self.iter.offset = offset;
                self.iter.pos += 1;
            }
            None => {
                self.iter.offset = usize::MAX;
                self.iter.pos    = usize::MAX;
            }
        }

        Some(item)
    }
}
