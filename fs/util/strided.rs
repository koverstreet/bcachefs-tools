// SPDX-License-Identifier: GPL-2.0

//! Arrays whose elements are a runtime stride apart, not size_of::<T>():
//! members_v2's, each member member_bytes - fewer than ours if an older
//! version wrote it, more if a newer one did.
//!
//! Element i is the stride bytes from i * stride, its slot. A slot reads as a
//! View - zero past the end of a short one, the rest of a long one ignored -
//! and is the T itself only if it holds all of it. Every slot is in the bytes,
//! checked at new(): an index past them is None, never a read past the end.

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
}
