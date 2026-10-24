//! Pointer masks: sets of one key's pointers, a bit per pointer by its index
//! among them - how the data update path names pointers (data_update_opts'
//! ptrs_kill, ptrs_kill_ec, ptrs_io_error), and what reconcile's plan and the
//! durability droppers pass around.
//!
//! A mask means nothing without its key, so a PtrMask<'id> carries the key's
//! brand: brand() hands its closure the key as a Branded<'id, K>, 'id fresh
//! and invariant, and masks are made only from it and used only with it.
//! remap() takes one to another key, finding the same pointers there.
//!
//! An edit that keeps the numbering - a pointer's cached bit, a stripe_ptr, a
//! cut, a crc - takes &mut Branded and keeps the brand. One that renumbers -
//! dropping a pointer; set_cached(), which can drop cached pointers and the
//! pointer itself; inserting one, which lands under its crc - consumes the
//! Branded or isn't Branded's, so the brand's masks have no key left. A copy
//! has its key's numbering: adopt() gives it the brand, checked. So every
//! Branded of a brand has the brand's numbering, each its own key: none is a
//! view of another's, which a renumbering through it would renumber under the
//! other.
//!
//! Masks stay within their key's pointers - assume() clips - so C's ~m is
//! except(m). At rest, in data_update_opts, a mask is raw bits relative to a
//! key nobody edits: bits() erases one, and assume() brands bits with their
//! key. Those two are where correctness is the caller's.

use super::*;
use crate::btree::bkey::BkeyFamily;
use core::marker::PhantomData;
use core::ops::{BitAnd, BitOr, BitOrAssign};

const _: () = assert!(c::BCH_BKEY_PTRS_MAX <= u16::BITS);

/// An invariant lifetime, standing for one key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Brand<'id>(PhantomData<fn(&'id ()) -> &'id ()>);

/// A set of the pointers of the key branded 'id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PtrMask<'id> {
    bits:  u16,
    brand: Brand<'id>,
}

impl<'id> PtrMask<'id> {
    fn of_bits(bits: u16) -> Self {
        PtrMask { bits, brand: Brand(PhantomData) }
    }

    /// Its raw bits, for a mask at rest: relative to this key, which whoever
    /// takes them back with assume() has to know.
    pub fn bits(self) -> u16 {
        self.bits
    }

    pub fn is_empty(self) -> bool {
        self.bits == 0
    }

    pub fn contains(self, i: usize) -> bool {
        i < 16 && self.bits & (1 << i) != 0
    }

    /// Its pointers' indices, each with its own one-pointer mask, lowest first.
    pub fn iter(self) -> impl DoubleEndedIterator<Item = (usize, PtrMask<'id>)> {
        (0..16).filter(move |&i| self.contains(i)).map(|i| (i, PtrMask::of_bits(1 << i)))
    }
}

impl BitOr for PtrMask<'_> {
    type Output = Self;
    fn bitor(self, m: Self) -> Self { PtrMask::of_bits(self.bits | m.bits) }
}

impl BitAnd for PtrMask<'_> {
    type Output = Self;
    fn bitand(self, m: Self) -> Self { PtrMask::of_bits(self.bits & m.bits) }
}

impl BitOrAssign for PtrMask<'_> {
    fn bitor_assign(&mut self, m: Self) { self.bits |= m.bits }
}

/// @k, branded 'id - for its closure only.
pub struct Branded<'id, K> {
    k:     K,
    brand: Brand<'id>,
}

/// @f, with @k branded: its masks can only be used with it, and @f's result
/// can't keep them.
pub fn brand<K: BkeyV<V: PtrsBkey>, R>(k: K, f: impl for<'id> FnOnce(Branded<'id, K>) -> R) -> R {
    f(Branded { k, brand: Brand(PhantomData) })
}

impl<'id, K: BkeyV<V: PtrsBkey>> Branded<'id, K> {
    /// The key, to read: masks aren't made from it, so it carries no brand.
    pub fn key(&self) -> BkeySC<'_, K::V> {
        self.k.as_sc()
    }

    pub fn into_inner(self) -> K {
        self.k
    }

    pub fn none(&self) -> PtrMask<'id> {
        PtrMask::of_bits(0)
    }

    pub fn all(&self, fs: &Fs) -> PtrMask<'id> {
        let n = self.k.ptrs(fs).count();
        PtrMask::of_bits(if n >= 16 { u16::MAX } else { (1 << n) - 1 })
    }

    /// Its pointers @pred says, called on each in order, once.
    pub fn where_(&self, fs: &Fs, mut pred: impl FnMut(&DecodedPtr<'_>) -> bool) -> PtrMask<'id> {
        let bits = self.k.ptrs_decoded(fs).take(16).enumerate()
            .filter(|(_, p)| pred(p))
            .fold(0, |m, (i, _)| m | 1 << i);
        PtrMask::of_bits(bits)
    }

    /// Its pointers not in @m.
    pub fn except(&self, fs: &Fs, m: PtrMask<'id>) -> PtrMask<'id> {
        PtrMask::of_bits(self.all(fs).bits & !m.bits)
    }

    /// @bits, taken against this key - the key at rest they're relative to -
    /// clipped to its pointers: past the last, a bit names none.
    pub fn assume(&self, fs: &Fs, bits: u32) -> PtrMask<'id> {
        PtrMask::of_bits(self.all(fs).bits & bits as u16)
    }

    /// The pointers of @m.
    pub fn ptrs_in<'a>(&'a self, fs: &'a Fs, m: PtrMask<'id>) -> impl Iterator<Item = &'a c::bch_extent_ptr> + 'a {
        let bits = m.bits;
        self.k.ptrs(fs).take(16).enumerate().filter(move |(i, _)| bits & (1 << i) != 0).map(|(_, p)| p)
    }

    /// @copy, a copy of this key, branded as it is: its pointers are the
    /// same, so are their indices.
    pub fn adopt<T: BkeyV<V: PtrsBkey>>(&self, fs: &Fs, copy: T) -> Branded<'id, T> {
        assert!(self.k.ptrs(fs).map(|p| (p.dev(), p.offset(), p.generation()))
                .eq(copy.ptrs(fs).map(|p| (p.dev(), p.offset(), p.generation()))),
                "adopting a copy of {} with other pointers: {}", self.k.to_text(fs), copy.to_text(fs));
        Branded { k: copy, brand: self.brand }
    }
}

/// The edits that keep the numbering.
impl<'id, K: BkeyVMut<V: PtrsBkey>> Branded<'id, K> {
    /// Its pointers, to edit in place - not to drop.
    pub fn ptrs_mut<'a>(&'a mut self, fs: &'a Fs) -> impl Iterator<Item = &'a mut c::bch_extent_ptr> + 'a {
        self.k.ptrs_mut(fs)
    }

    /// Its entries, to drop a stripe_ptr from: a key with one has entries
    /// that can be edited.
    fn entries_mut(&mut self, fs: &Fs) -> BkeyS<'_, PtrsMut> {
        assert!(PtrsMut::contains(self.k.k()), "dropping a stripe_ptr of a key whose entries can't be: {}",
                self.k.to_text(fs));
        self.k.as_s().arm::<PtrsMut>().unwrap()
    }

    pub fn drop_ec(&mut self, fs: &Fs, dev: u32) {
        self.entries_mut(fs).drop_ec(fs, dev);
    }

    /// drop_ec() for each pointer of @m.
    pub fn drop_ec_mask(&mut self, fs: &Fs, m: PtrMask<'id>) {
        let devs: ArrayVec<u32, { c::BCH_BKEY_PTRS_MAX as usize }> =
            self.ptrs_in(fs, m).map(|p| u32::from(p.dev())).collect();

        for dev in devs {
            self.drop_ec(fs, dev);
        }
    }
}

/// The edits that renumber: the key comes back unbranded.
impl<'id, K: BkeyVMut<V: PtrsBkey>> Branded<'id, K> {
    /// The pointers of @m dropped - the highest first, so the lower are where
    /// they were.
    pub fn drop_ptrs_mask(mut self, fs: &Fs, m: PtrMask<'id>) -> K {
        for (i, _) in m.iter().rev() {
            let at = self.k.ptrs_decoded(fs).nth(i).map(|p| p.pos)
                .expect("a mask's pointer, within its key's");
            self.k.drop_ptr_noerror(fs, at);
        }
        self.k
    }

    /// set_cached() each pointer of @m, the highest first. The mask is read as
    /// devices before any is made cached - set_cached() can drop cached
    /// pointers anywhere in the key, moving the ones after them.
    pub fn set_ptrs_cached_mask(mut self, fs: &Fs, opts: &c::bch_inode_opts, m: PtrMask<'id>) -> K {
        let devs: ArrayVec<u32, { c::BCH_BKEY_PTRS_MAX as usize }> =
            self.ptrs_in(fs, m).map(|p| u32::from(p.dev())).collect();

        for dev in devs.into_iter().rev() {
            self.k.set_cached(fs, opts, dev);
        }
        self.k
    }
}
