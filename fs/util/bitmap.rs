// SPDX-License-Identifier: GPL-2.0

//! Bitmaps: C's `unsigned long d[BITS_TO_LONGS(n)]`, and the bitops on it.
//!
//! C uses one array two ways, so here it's two types. Bitmap is the plain
//! kind - __set_bit() and friends, on a mask nothing else can see: one on the
//! stack, an allocation request's. AtomicBitmap is the shared kind - set_bit()
//! and friends, with readers going at it meanwhile: c->devs_online, the
//! allocator's rw_devs[], write completions marking devs_need_flush. Between
//! them is the snapshot C takes by copying the struct - `new_rw_devs =
//! c->allocator.rw_devs[0]`, then __clear_bit() on the copy - which here is
//! AtomicBitArray::load(), into a BitArray to change.
//!
//! Both are unsized, over their words, as the kernel's Rust bitmap is; a
//! fixed-size one is a BitArray or AtomicBitArray, which derefs to it. Sized
//! in words, not bits: a [usize; bits_to_longs(BITS)] with BITS a generic
//! parameter needs generic_const_exprs, which is unstable - so a field says
//! its size where it's declared, BitArray<{ bits_to_longs(N) }>.
//!
//! Nothing here sets a bit past the ones a bitmap is for, so weight() and
//! iter() needn't know where those end; the one thing that looks for a clear
//! bit, next_zero(), is told.
//!
//! Orderings follow C's bitops: set, clear and test are relaxed, and those
//! that return the old value - test_and_set_bit() - are fully ordered.

use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};

const BITS: usize = usize::BITS as usize;

/// C's words are unsigned longs.
const _: () = assert!(size_of::<core::ffi::c_ulong>() == size_of::<usize>());

/// Bit @bit's word, and its mask in that word.
fn pos(bit: usize) -> (usize, usize) {
    (bit / BITS, 1 << (bit % BITS))
}

/// The set bits of word @i, @w, in order: for_each_set_bit()'s inner loop.
fn word_bits(i: usize, mut w: usize) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        (w != 0).then(|| {
            let b = w.trailing_zeros() as usize;
            w &= w - 1;
            i * BITS + b
        })
    })
}

// ── Plain ────────────────────────────────────────────────────────────────

/// A bitmap nothing else is changing: __set_bit(), __clear_bit(), bitmap_*().
#[repr(transparent)]
pub struct Bitmap([usize]);

impl Bitmap {
    pub fn from_words(w: &[usize]) -> &Bitmap {
        // SAFETY: repr(transparent) over [usize]
        unsafe { &*(w as *const [usize] as *const Bitmap) }
    }

    pub fn from_words_mut(w: &mut [usize]) -> &mut Bitmap {
        // SAFETY: repr(transparent) over [usize]
        unsafe { &mut *(w as *mut [usize] as *mut Bitmap) }
    }

    pub fn words(&self) -> &[usize] {
        &self.0
    }

    /// test_bit(): false past the end.
    pub fn test(&self, bit: usize) -> bool {
        let (i, m) = pos(bit);
        self.0.get(i).is_some_and(|w| w & m != 0)
    }

    /// __set_bit().
    pub fn set(&mut self, bit: usize) {
        let (i, m) = pos(bit);
        self.0[i] |= m;
    }

    /// __clear_bit().
    pub fn clear(&mut self, bit: usize) {
        let (i, m) = pos(bit);
        self.0[i] &= !m;
    }

    /// __test_and_set_bit(): whether it was set.
    pub fn test_and_set(&mut self, bit: usize) -> bool {
        let was = self.test(bit);
        self.set(bit);
        was
    }

    /// bitmap_weight().
    pub fn weight(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }

    /// bitmap_empty().
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&w| w == 0)
    }

    /// The set bits, in order: for_each_set_bit().
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(i, &w)| word_bits(i, w))
    }

    /// The first clear bit from @start, before @end: find_next_zero_bit().
    pub fn next_zero(&self, start: usize, end: usize) -> Option<usize> {
        (start..end).find(|&bit| !self.test(bit))
    }

    fn zip_mut(&mut self, o: &Bitmap, f: impl Fn(usize, usize) -> usize) {
        assert_eq!(self.0.len(), o.0.len(), "bitmaps of different sizes");
        for (a, &b) in self.0.iter_mut().zip(&o.0) {
            *a = f(*a, b);
        }
    }

    /// bitmap_and(), into this one.
    pub fn and(&mut self, o: &Bitmap) {
        self.zip_mut(o, |a, b| a & b)
    }

    /// bitmap_or(), into this one.
    pub fn or(&mut self, o: &Bitmap) {
        self.zip_mut(o, |a, b| a | b)
    }

    /// bitmap_andnot(), into this one: @o's bits cleared.
    pub fn andnot(&mut self, o: &Bitmap) {
        self.zip_mut(o, |a, b| a & !b)
    }

    /// bitmap_intersects().
    pub fn intersects(&self, o: &Bitmap) -> bool {
        self.0.iter().zip(&o.0).any(|(a, b)| a & b != 0)
    }
}

impl PartialEq for Bitmap {
    /// bitmap_equal().
    fn eq(&self, o: &Bitmap) -> bool {
        self.0 == o.0
    }
}

impl Eq for Bitmap {}

/// A fixed-size Bitmap, of @W words: C's `unsigned long d[W]`.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BitArray<const W: usize>([usize; W]);

impl<const W: usize> Default for BitArray<W> {
    fn default() -> Self {
        Self([0; W])
    }
}

impl<const W: usize> Deref for BitArray<W> {
    type Target = Bitmap;

    fn deref(&self) -> &Bitmap {
        Bitmap::from_words(&self.0)
    }
}

impl<const W: usize> DerefMut for BitArray<W> {
    fn deref_mut(&mut self) -> &mut Bitmap {
        Bitmap::from_words_mut(&mut self.0)
    }
}

// ── Atomic ───────────────────────────────────────────────────────────────

/// A bitmap that's set and cleared under readers: set_bit(), clear_bit().
/// Each bit is atomic - a reader walking it sees each word as it was when
/// it got there, not the whole as of one moment.
#[repr(transparent)]
pub struct AtomicBitmap([AtomicUsize]);

impl AtomicBitmap {
    pub fn from_words(w: &[AtomicUsize]) -> &AtomicBitmap {
        // SAFETY: repr(transparent) over [AtomicUsize]
        unsafe { &*(w as *const [AtomicUsize] as *const AtomicBitmap) }
    }

    /// test_bit(): false past the end.
    pub fn test(&self, bit: usize) -> bool {
        let (i, m) = pos(bit);
        self.0.get(i).is_some_and(|w| w.load(Ordering::Relaxed) & m != 0)
    }

    /// set_bit().
    pub fn set(&self, bit: usize) {
        let (i, m) = pos(bit);
        self.0[i].fetch_or(m, Ordering::Relaxed);
    }

    /// clear_bit().
    pub fn clear(&self, bit: usize) {
        let (i, m) = pos(bit);
        self.0[i].fetch_and(!m, Ordering::Relaxed);
    }

    /// Set if @v, else clear: mod_bit().
    pub fn assign(&self, bit: usize, v: bool) {
        if v { self.set(bit) } else { self.clear(bit) }
    }

    /// test_and_set_bit(): whether it was set.
    pub fn test_and_set(&self, bit: usize) -> bool {
        let (i, m) = pos(bit);
        self.0[i].fetch_or(m, Ordering::SeqCst) & m != 0
    }

    /// test_and_clear_bit(): whether it was set.
    pub fn test_and_clear(&self, bit: usize) -> bool {
        let (i, m) = pos(bit);
        self.0[i].fetch_and(!m, Ordering::SeqCst) & m != 0
    }

    /// bitmap_weight().
    pub fn weight(&self) -> u32 {
        self.0.iter().map(|w| w.load(Ordering::Relaxed).count_ones()).sum()
    }

    /// The set bits, in order, each word read as it's reached:
    /// for_each_set_bit().
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(i, w)| word_bits(i, w.load(Ordering::Relaxed)))
    }
}

/// A fixed-size AtomicBitmap, of @W words: C's `unsigned long d[W]`, set_bit()
/// on.
#[repr(transparent)]
pub struct AtomicBitArray<const W: usize>([AtomicUsize; W]);

impl<const W: usize> AtomicBitArray<W> {
    /// A copy to work on - C's struct copy of a shared mask.
    pub fn load(&self) -> BitArray<W> {
        BitArray(core::array::from_fn(|i| self.0[i].load(Ordering::Relaxed)))
    }

    /// Replace it with @b, a word at a time - readers can see a mix.
    pub fn store(&self, b: &BitArray<W>) {
        for (a, &b) in self.0.iter().zip(&b.0) {
            a.store(b, Ordering::Relaxed);
        }
    }
}

impl<const W: usize> Default for AtomicBitArray<W> {
    fn default() -> Self {
        Self([const { AtomicUsize::new(0) }; W])
    }
}

impl<const W: usize> Deref for AtomicBitArray<W> {
    type Target = AtomicBitmap;

    fn deref(&self) -> &AtomicBitmap {
        AtomicBitmap::from_words(&self.0)
    }
}
