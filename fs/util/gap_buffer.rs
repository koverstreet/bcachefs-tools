// SPDX-License-Identifier: GPL-2.0

//! A gap buffer: an array whose free space is at @gap rather than its end, so
//! an insert near the last one moves only what's between the two - sorted
//! inserts, as recovery makes into the journal keys, are O(n) in all, not
//! O(n) each. Laid out as btree/journal_overlay_types.rs's journal_keys has
//! it, a darray's nr, size and data, then gap, while C reads those.
//!
//! An element's index is its place in the sequence, 0..len(); where it's
//! stored is the index, or past the gap the index plus the gap's size. Like a
//! DArray it doesn't drop its elements, which are their owner's; the storage
//! is a KVVec's, kvmalloc()'d, as C's is, and freed as one. Growing is
//! reserve()'s, apart from insert(), so an element that owns something is
//! never handed over to an allocation that fails.

use crate::util::alloc::{flags::GFP_KERNEL, AllocError, KVVec};
use core::ops::Index;
use core::ptr;

#[repr(C)]
pub struct GapBuffer<T> {
    pub nr:           usize,
    pub size:         usize,
    pub data:         *mut T,
    pub preallocated: [T; 0],
    pub gap:          usize,
}

impl<T> GapBuffer<T> {
    fn free(&self) -> usize {
        self.size - self.nr
    }

    fn idx_to_pos(&self, idx: usize) -> usize {
        if idx >= self.gap { idx + self.free() } else { idx }
    }

    pub fn len(&self) -> usize {
        self.nr
    }

    pub fn is_empty(&self) -> bool {
        self.nr == 0
    }

    pub fn get(&self, idx: usize) -> Option<&T> {
        // SAFETY: an element's position
        (idx < self.nr).then(|| unsafe { &*self.data.add(self.idx_to_pos(idx)) })
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut T> {
        // SAFETY: as get()'s
        (idx < self.nr).then(|| unsafe { &mut *self.data.add(self.idx_to_pos(idx)) })
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.nr).map(|i| &self[i])
    }

    /// The gap moved to before index @gap: move_gap().
    fn move_gap(&mut self, gap: usize) {
        assert!(gap <= self.nr && self.gap <= self.nr, "gap {gap}, was {}, of {}", self.gap, self.nr);
        let (old, free) = (self.gap, self.free());
        // SAFETY: what moves is elements, into the gap's room, which they fit
        unsafe {
            if gap < old {
                ptr::copy(self.data.add(gap), self.data.add(gap + free), old - gap);
            } else if gap > old {
                ptr::copy(self.data.add(old + free), self.data.add(old), gap - old);
            }
        }
        self.gap = gap;
    }

    /// The elements, one slice: the gap moved to the end.
    pub fn make_contiguous(&mut self) -> &mut [T] {
        self.move_gap(self.nr);
        if self.data.is_null() {
            return &mut [];
        }
        // SAFETY: the gap at the end, the elements are the first nr
        unsafe { core::slice::from_raw_parts_mut(self.data, self.nr) }
    }

    /// The first @len kept, the rest forgotten - their owner's, as
    /// make_contiguous() left them.
    pub fn truncate(&mut self, len: usize) {
        self.move_gap(self.nr);
        self.nr = self.nr.min(len);
        self.gap = self.nr;
    }

    /// Room for one more: if it's full, grown to twice the size, the gap at
    /// the end.
    pub fn reserve(&mut self) -> Result<(), AllocError> {
        if self.nr < self.size {
            return Ok(());
        }

        let size = self.size.max(8) * 2;
        let mut new = KVVec::<T>::with_capacity(size, GFP_KERNEL)?;
        // SAFETY: full, so no gap: nr elements, into room for size
        unsafe {
            if self.nr != 0 {
                ptr::copy_nonoverlapping(self.data, new.as_mut_ptr(), self.nr);
            }
            drop(self.take_storage());
        }
        let (data, _, _) = new.into_raw_parts();
        self.data = data;
        self.size = size;
        self.gap = self.nr;
        Ok(())
    }

    /// @v inserted at index @idx, into room reserve() made.
    pub fn insert(&mut self, idx: usize, v: T) {
        assert!(idx <= self.nr && self.nr < self.size, "insert at {idx} of {}, size {}", self.nr, self.size);
        self.move_gap(idx);
        // SAFETY: the gap's first slot, within size
        unsafe { self.data.add(self.gap).write(v) };
        self.gap += 1;
        self.nr += 1;
    }

    /// The storage, as the KVVec it was allocated as, its elements not
    /// counted: whoever drops it frees only memory.
    ///
    /// # Safety
    /// The elements are moved out, or theirs is to drop no more.
    unsafe fn take_storage(&mut self) -> Option<KVVec<T>> {
        let data = core::mem::replace(&mut self.data, ptr::null_mut());
        // SAFETY: a KVVec's, of capacity size
        (!data.is_null()).then(|| unsafe { KVVec::from_raw_parts(data, 0, self.size) })
    }

    /// The storage freed - not its elements: kvfree() of C's data.
    pub fn free_storage(&mut self) {
        // SAFETY: the elements are their owner's, not dropped here
        drop(unsafe { self.take_storage() });
        (self.nr, self.size, self.gap) = (0, 0, 0);
    }
}

impl<T> Index<usize> for GapBuffer<T> {
    type Output = T;

    fn index(&self, idx: usize) -> &T {
        self.get(idx).unwrap_or_else(|| panic!("index {idx} of {}", self.nr))
    }
}

// SAFETY: zeroed is empty, nothing allocated - as DArray's
unsafe impl<T> ::pin_init::Zeroable for GapBuffer<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inserts anywhere, through growth, give the sequence in order.
    #[test]
    fn insert() {
        let mut b: GapBuffer<u32> = ::pin_init::zeroed();
        let mut want = Vec::new();

        let mut x = 0x2545f4914f6cdd1du64;
        for i in 0..1000u32 {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            let idx = if x % 4 == 0 { (x >> 8) as usize % (want.len() + 1) } else { want.len().min(b.gap + 1) };
            b.reserve().unwrap();
            b.insert(idx, i);
            want.insert(idx, i);
        }
        assert_eq!(b.iter().copied().collect::<Vec<_>>(), want);

        assert_eq!(b.make_contiguous(), &want[..]);
        b.truncate(10);
        assert_eq!(b.make_contiguous(), &want[..10]);
        b.free_storage();
    }
}
