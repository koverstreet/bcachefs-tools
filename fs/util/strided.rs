// SPDX-License-Identifier: GPL-2.0

//! Arrays whose elements are a runtime stride apart, not size_of::<T>():
//! members_v2's, each member member_bytes - fewer than ours if an older
//! version wrote it, more if a newer one did.
//!
//! Element i is the stride bytes from i * stride, its slot. A slot reads as a
//! View - zero past the end of a short one, the rest of a long one ignored -
//! and is the T itself only if it holds all of it. Every slot is in the bytes,
//! checked at new(): an index past them is None, never a read past the end.
//!
//! StridedBuf owns its slots, for a table built and rebuilt in memory -
//! c->replicas, its entries all the size of the biggest. What a slot holds is
//! its owner's to read; an index is one the owner has, so past the end is a
//! bug, as a slice's.

use crate::util::alloc::{flags, AllocError, KVVec};
use crate::util::vstructs::{cast_plain_mut, Plain, View};
use core::marker::PhantomData;

pub struct Strided<'a, T> {
    bytes:  &'a [u8],
    stride: usize,
    _t:     PhantomData<&'a T>,
}

pub struct StridedMut<'a, T> {
    bytes:  &'a mut [u8],
    stride: usize,
    _t:     PhantomData<&'a mut T>,
}

fn slots_len(len: usize, stride: usize) -> Option<usize> {
    (stride != 0).then(|| len / stride * stride)
}

impl<'a, T: Plain> Strided<'a, T> {
    /// None if @stride is 0.
    pub fn new(bytes: &'a [u8], stride: usize) -> Option<Self> {
        let len = slots_len(bytes.len(), stride)?;
        Some(Strided { bytes: &bytes[..len], stride, _t: PhantomData })
    }

    pub fn len(&self) -> usize {
        self.bytes.len() / self.stride
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn get(&self, i: usize) -> Option<View<'a, T>> {
        let at = i.checked_mul(self.stride)?;
        self.bytes.get(at..at + self.stride).map(View::new)
    }
}

impl<'a, T: Plain> StridedMut<'a, T> {
    /// None if @stride is 0.
    pub fn new(bytes: &'a mut [u8], stride: usize) -> Option<Self> {
        let len = slots_len(bytes.len(), stride)?;
        Some(StridedMut { bytes: &mut bytes[..len], stride, _t: PhantomData })
    }

    /// @nr slots re-laid out from @from bytes apart to @to, top down so none
    /// is overwritten before it moves, each one's new bytes zeroed.
    pub fn grow(bytes: &'a mut [u8], nr: usize, from: usize, to: usize) -> Option<Self> {
        if from == 0 || to < from || nr.checked_mul(to)? > bytes.len() {
            return None;
        }
        for i in (0..nr).rev() {
            bytes.copy_within(i * from..(i + 1) * from, i * to);
            bytes[i * to + from..(i + 1) * to].fill(0);
        }
        Self::new(&mut bytes[..nr * to], to)
    }

    pub fn slot_bytes_mut(&mut self, i: usize) -> Option<&mut [u8]> {
        let at = i.checked_mul(self.stride)?;
        self.bytes.get_mut(at..at + self.stride)
    }

    /// Element @i itself, if its slot holds all of it.
    pub fn into_whole_mut(self, i: usize) -> Option<&'a mut T> {
        let at = i.checked_mul(self.stride)?;
        cast_plain_mut(self.bytes.get_mut(at..at + self.stride)?)
    }
}

pub struct StridedBuf {
    bytes:  KVVec<u8>,
    stride: usize,
}

impl StridedBuf {
    pub const fn new() -> Self {
        StridedBuf { bytes: KVVec::new(), stride: 0 }
    }

    /// @nr slots of @stride bytes, zeroed.
    pub fn zeroed(nr: usize, stride: usize) -> Result<Self, AllocError> {
        let len = nr.checked_mul(stride).ok_or(AllocError)?;
        Ok(StridedBuf { bytes: KVVec::from_elem(0, len, flags::GFP_KERNEL)?, stride })
    }

    pub fn len(&self) -> usize {
        self.bytes.len().checked_div(self.stride).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn slot(&self, i: usize) -> &[u8] {
        &self.bytes[i * self.stride..(i + 1) * self.stride]
    }

    pub fn slot_mut(&mut self, i: usize) -> &mut [u8] {
        &mut self.bytes[i * self.stride..(i + 1) * self.stride]
    }

    pub fn slots(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).map(|i| self.slot(i))
    }

    pub fn swap(&mut self, i: usize, j: usize) {
        if i != j {
            let (lo, hi) = (i.min(j), i.max(j));
            let (a, b) = self.bytes.split_at_mut(hi * self.stride);
            a[lo * self.stride..(lo + 1) * self.stride].swap_with_slice(&mut b[..self.stride]);
        }
    }

    /// Slot @from's bytes, over slot @to's.
    pub fn copy(&mut self, from: usize, to: usize) {
        self.bytes.copy_within(from * self.stride..(from + 1) * self.stride, to * self.stride);
    }

    pub fn truncate(&mut self, nr: usize) {
        self.bytes.truncate(nr * self.stride);
    }

    /// A copy, @extra slots more, each @stride bytes - no fewer than ours:
    /// each slot of ours at the start of its, the rest zero.
    pub fn restrided(&self, stride: usize, extra: usize) -> Result<Self, AllocError> {
        assert!(stride >= self.stride);
        let mut new = Self::zeroed(self.len() + extra, stride)?;
        for (i, s) in self.slots().enumerate() {
            new.slot_mut(i)[..s.len()].copy_from_slice(s);
        }
        Ok(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::vstructs::{plain_bytes, plain_bytes_mut};

    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug, PartialEq, cstruct_macros::Plain)]
    struct Pair {
        a: u32,
        b: u32,
    }

    /// A short slot reads as zero past its end, a long one's extra bytes are
    /// skipped, and only a slot that holds all of one is one.
    #[test]
    fn strides() {
        let w: [u32; 3] = [1, 2, 3];
        let s = Strided::<Pair>::new(plain_bytes(&w[..]), 4).unwrap();
        assert_eq!(s.get(1).unwrap().get(), Pair { a: 2, b: 0 });
        assert!(s.get(3).is_none());

        let mut w: [u32; 6] = [1, 2, 9, 3, 4, 9];
        let s = Strided::<Pair>::new(plain_bytes(&w[..]), 12).unwrap();
        assert_eq!(s.get(1).unwrap().get(), Pair { a: 3, b: 4 });
        assert!(Strided::<Pair>::new(plain_bytes(&w[..]), 0).is_none());

        let s = StridedMut::<Pair>::new(plain_bytes_mut(&mut w[..]), 12).unwrap();
        s.into_whole_mut(1).unwrap().a = 7;
        assert_eq!(w, [1, 2, 9, 7, 4, 9]);
        assert!(StridedMut::<Pair>::new(plain_bytes_mut(&mut w[..]), 4).unwrap().into_whole_mut(0).is_none());
    }

    /// Each slot keeps its bytes, zero past them; past the slots, nothing's
    /// touched.
    #[test]
    fn grow() {
        let mut w: [u32; 7] = [1, 2, 3, 9, 9, 9, 8];
        StridedMut::<Pair>::grow(plain_bytes_mut(&mut w[..]), 3, 4, 8).unwrap();
        assert_eq!(w, [1, 0, 2, 0, 3, 0, 8]);

        let mut w: [u32; 4] = [1, 2, 3, 4];
        assert!(StridedMut::<Pair>::grow(plain_bytes_mut(&mut w[..]), 3, 4, 8).is_none());
        assert!(StridedMut::<Pair>::grow(plain_bytes_mut(&mut w[..]), 2, 8, 4).is_none());
        assert_eq!(w, [1, 2, 3, 4]);
    }

    /// A restride keeps each slot's bytes at its start, zero after; swap and
    /// copy move whole slots.
    #[test]
    fn buf() {
        let mut b = StridedBuf::zeroed(3, 2).unwrap();
        for i in 0..3 {
            b.slot_mut(i).copy_from_slice(&[i as u8 + 1, 9]);
        }
        b.swap(0, 2);
        b.copy(1, 0);
        assert!(b.slots().eq([&[2, 9][..], &[2, 9], &[1, 9]]));

        let r = b.restrided(3, 1).unwrap();
        assert!(r.slots().eq([&[2, 9, 0][..], &[2, 9, 0], &[1, 9, 0], &[0, 0, 0]]));

        b.truncate(1);
        assert_eq!(b.len(), 1);
        assert_eq!(StridedBuf::new().len(), 0);
    }
}
