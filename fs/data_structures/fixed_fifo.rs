// SPDX-License-Identifier: GPL-2.0

//! A fifo whose entries are its slots: N of them, made with it and dropped
//! with it, never in between. Pushing and popping move only the indices, and
//! hand back the slot taken in or let go, for its owner to fill or empty in
//! place. So an entry never moves, and a slot is always a T - for what reads
//! it whether it's an entry or not, and for what's placed there: it's an
//! Array.
//!
//! Indices are absolute, as Fifo's are: front and back count up forever,
//! wrapping at I's width, and index i is slot i % N - N a power of two, so
//! that holds across the wrap.

use crate::data_structures::fifo::FifoIdx;
use crate::util::place::Array;
use pin_init::{pin_data, pin_init, PinInit};

#[pin_data]
#[repr(C)]
pub struct FixedFifo<T, const N: usize, I = usize> {
    front: I,
    back:  I,
    #[pin]
    slots: [T; N],
}

impl<T, const N: usize, I: FifoIdx> FixedFifo<T, N, I> {
    const POW2: () = assert!(N.is_power_of_two(), "a FixedFifo's size is a power of two");

    /// Empty, at index 0, each slot made by @make, given its number.
    pub fn new<S: PinInit<T, E>, E>(make: impl FnMut(usize) -> S) -> impl PinInit<Self, E> {
        let () = Self::POW2;
        pin_init!(Self { front: I::ZERO, back: I::ZERO, slots <- pin_init::pin_init_array_from_fn(make) }? E)
    }

    fn add(i: I, n: usize) -> I {
        i.wrapping_add(&I::from_usize(n))
    }

    /// Slot @i, entry or not.
    pub fn slot(&self, i: I) -> &T {
        &self.slots[i.as_() & (N - 1)]
    }

    fn slot_mut(&mut self, i: I) -> &mut T {
        &mut self.slots[i.as_() & (N - 1)]
    }

    pub fn len(&self) -> usize {
        self.back.wrapping_sub(&self.front).as_()
    }

    pub fn is_empty(&self) -> bool {
        self.front == self.back
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    pub fn free(&self) -> usize {
        N - self.len()
    }

    pub fn is_full(&self) -> bool {
        self.len() == N
    }

    pub fn front_idx(&self) -> I {
        self.front
    }

    /// One past the last entry's index: the next push_back()'s.
    pub fn back_idx(&self) -> I {
        self.back
    }

    /// Empty, at index @i: as it starts numbering entries.
    pub fn start_at(&mut self, i: I) {
        assert!(self.is_empty(), "restarting a fixed fifo with entries");
        self.front = i;
        self.back = i;
    }

    fn live(&self, i: I) -> bool {
        i.wrapping_sub(&self.front).as_() < self.len()
    }

    pub fn get(&self, i: I) -> Option<&T> {
        self.live(i).then(|| self.slot(i))
    }

    pub fn get_mut(&mut self, i: I) -> Option<&mut T> {
        self.live(i).then(|| self.slot_mut(i))
    }

    pub fn peek_front(&self) -> Option<&T> {
        self.get(self.front)
    }

    /// The slot at back, an entry now - None if full.
    pub fn push_back(&mut self) -> Option<&mut T> {
        if self.is_full() {
            return None;
        }
        let i = self.back;
        self.back = Self::add(i, 1);
        Some(self.slot_mut(i))
    }

    /// The slot at front, an entry no longer - None if empty.
    pub fn pop_front(&mut self) -> Option<&mut T> {
        if self.is_empty() {
            return None;
        }
        let i = self.front;
        self.front = Self::add(i, 1);
        Some(self.slot_mut(i))
    }

    /// The entries, front to back.
    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        (0..self.len()).map(move |k| self.slot(Self::add(self.front, k)))
    }
}

unsafe impl<T, const N: usize, I: FifoIdx> Array for FixedFifo<T, N, I> {
    type Elem = T;
    const ELEMS_OFFSET: usize = core::mem::offset_of!(Self, slots);

    fn init_each<S: PinInit<T, E>, E>(make: impl FnMut(usize) -> S) -> impl PinInit<Self, E> {
        Self::new(make)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_persist_across_wrap() {
        pin_init::stack_pin_init!(let f = FixedFifo::<u32, 4, u16>::new(|i| i as u32));
        let f = f.get_mut();

        assert_eq!(f.pop_front(), None);

        /* past the u16 wrap: a slot has what it was made with, then what its last entry left */
        f.start_at(u16::MAX - 1);
        for n in 0..8 {
            let i = f.back_idx();
            let slot = i as u32 % 4;
            let s = f.push_back().unwrap();
            assert_eq!(*s, if n < 4 { slot } else { 100 + slot });
            *s = 100 + slot;
            assert_eq!(f.get(i), Some(&(100 + slot)));
            assert_eq!(f.pop_front().copied(), Some(100 + slot));
        }
        assert!(f.is_empty());

        for _ in 0..4 {
            f.push_back().unwrap();
        }
        assert!(f.push_back().is_none());
        assert_eq!(f.iter().count(), 4);
    }
}
