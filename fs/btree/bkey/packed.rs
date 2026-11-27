// SPDX-License-Identifier: GPL-2.0

//! BkeyP, a key as a btree node holds it: mod.rs has how it fits with the
//! other forms.

use core::cmp::Ordering;
use core::marker::PhantomData;
use crate::btree::bkey::{types::{cast_unsized, cast_unsized_mut}, AnyKey, BKEY_U64S, BkeySC};
use crate::btree::node::NodeContents;
use crate::format::{Bkey, BkeyFormat, BkeyFormatBits, BkeyPacked, BkeyType, Bpos,
    KEY_FORMAT_CURRENT};
use crate::os::kernel::likely;
use crate::util::vstructs::{cast_plain_slice, cast_plain_slice_mut, plain_bytes, plain_bytes_mut,
    State, Unsized, Unvalidated};
use cstruct_macros::{unsized_vstruct, MaybeZeroable, Plain};

/// A key as a node holds it, in state S: its header, then the rest of its
/// u64s - as many as its header says, the header's included. The key's own
/// fields are in the node's format and its value follows them, so neither is
/// a type: a packed key is worked with unpacked - disassemble() is the key in
/// S, its value any type's, to narrow.
///
/// Its header, Head<BkeyP>: its first u64, its u64s, format and type the first
/// three bytes. BkeyPacked itself is padded to a bkey's size, so a reference
/// to one reaches past a short key. What every key has, so what's read of a
/// key before its u64s are checked.
#[unsized_vstruct(tail = [u64], u64s = u64s, start = 0)]
#[repr(C, align(8))]
#[derive(MaybeZeroable, Plain)]
pub struct BkeyP {
    pub(super) u64s:        u8,
    pub(super) format_bits: BkeyFormatBits,
    pub(super) type_:       u8,
    _key:                   [u8; 5],
}

impl BkeyPHead {
    pub(super) fn of(w: &u64) -> &Self {
        unsafe { &*(w as *const u64).cast() }
    }

    pub(super) fn of_mut(w: &mut u64) -> &mut Self {
        unsafe { &mut *(w as *mut u64).cast() }
    }

    pub fn format(&self) -> u8 {
        self.format_bits.format()
    }

    pub fn is_packed(&self) -> bool {
        let format = self.format() as u32;
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(format <= KEY_FORMAT_CURRENT, "key of format {format}");
        }
        format != KEY_FORMAT_CURRENT
    }

    pub fn key_u64s(&self, f: &BkeyFormat) -> usize {
        if self.is_packed() { f.key_u64s as usize } else { BKEY_U64S }
    }

    /// Its value fits a bkey_i's, once unpacked.
    pub fn u64s_valid(&self, f: &BkeyFormat) -> bool {
        self.u64s()
            .checked_sub(self.key_u64s(f))
            .is_some_and(|v| v <= u8::MAX as usize - BKEY_U64S)
    }
}

impl BkeyP {
    /// The key at @p, as long as its u64s say - its header, if they say
    /// it's shorter.
    pub unsafe fn from_raw(p: *const BkeyPacked) -> *const Self {
        <Self as Unsized>::from_raw(p.cast(), unsafe { (*p).u64s as usize }.max(1) * size_of::<u64>())
    }

    pub unsafe fn from_raw_mut(p: *mut BkeyPacked) -> *mut Self {
        <Self as Unsized>::from_raw_mut(p.cast(), unsafe { (*p).u64s as usize }.max(1) * size_of::<u64>())
    }
}

impl BkeyP<Unvalidated> {
    /// The key @s starts with, if all of it is in @s.
    pub fn from_slice(s: &[u64]) -> Option<&Self> {
        Self::from_bytes(plain_bytes(s)).ok()
    }

    /// A key in a BkeyPacked of its own, as one packed onto the stack.
    pub fn from_packed(k: &BkeyPacked) -> Option<&Self> {
        Self::from_slice(cast_plain_slice(plain_bytes(k)).expect("a bkey_packed is whole u64s"))
    }

    pub fn from_slice_mut(s: &mut [u64]) -> Option<&mut Self> {
        Self::from_bytes_mut(plain_bytes_mut(s)).ok()
    }
}

impl<S: State> BkeyP<S> {
    /// Its u64s, the header's included.
    pub fn as_slice(&self) -> &[u64] {
        cast_plain_slice(self.as_bytes()).expect("a packed key is whole u64s")
    }

    pub(crate) fn cast<T: State>(&self) -> &BkeyP<T> {
        unsafe { &*(self as *const Self as *const BkeyP<T>) }
    }

    pub(crate) fn cast_mut<T: State>(&mut self) -> &mut BkeyP<T> {
        unsafe { &mut *(self as *mut Self as *mut BkeyP<T>) }
    }

    pub fn bytes(&self) -> usize {
        self.u64s() * size_of::<u64>()
    }

    pub fn needs_whiteout(&self) -> bool {
        self.format_bits.needs_whiteout() != 0
    }

    pub fn set_needs_whiteout(&mut self, v: bool) {
        self.format_bits.set_needs_whiteout(v as u8);
    }

    pub fn set_format(&mut self, format: u8) {
        self.format_bits.set_format(format);
    }

    pub fn key_type(&self) -> BkeyType {
        BkeyType(self.type_ as u32)
    }

    pub fn set_key_type(&mut self, t: BkeyType) {
        self.type_ = t.0 as u8;
    }

    pub fn key_bytes(&self, f: &BkeyFormat) -> usize {
        self.key_u64s(f) * size_of::<u64>()
    }

    fn is_bkey_i(&self) -> bool {
        !self.is_packed() && self.u64s() >= BKEY_U64S
    }
}

/// Reading past the header - the value, or an unpacked key's whole header,
/// a bkey - needs the key's u64s to be validate's.
impl BkeyP {
    pub fn val_u64s(&self, f: &BkeyFormat) -> usize {
        self.u64s() - self.key_u64s(f)
    }

    pub fn val_bytes(&self, f: &BkeyFormat) -> usize {
        self.val_u64s(f) * size_of::<u64>()
    }

    pub fn val(&self, f: &BkeyFormat) -> &[u64] {
        let (key_u64s, words) = (self.key_u64s(f), self.as_slice());
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(key_u64s <= words.len());
        }
        unsafe { words.get_unchecked(key_u64s..) }
    }

    pub fn val_mut(&mut self, f: &BkeyFormat) -> &mut [u64] {
        let key_u64s = self.key_u64s(f);
        let words = cast_plain_slice_mut(self.as_bytes_mut()).expect("a packed key is whole u64s");
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(key_u64s <= words.len());
        }
        unsafe { words.get_unchecked_mut(key_u64s..) }
    }

    fn unpacked_header(&self) -> &Bkey {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(self.is_bkey_i());
        }
        unsafe { &*core::ptr::from_ref(self).cast::<Bkey>() }
    }

    /// Just the key, not the value.
    pub fn unpack(&self, b: &NodeContents) -> Bkey {
        if likely(self.is_packed()) {
            b.format.unpack_key(self)
        } else {
            *self.unpacked_header()
        }
    }

    /// The key and the value, into @dst.
    pub fn unpack_into(&self, b: &NodeContents, dst: &mut [u64]) {
        self.unpacked_into(&b.format, self.unpack(b), dst);
    }

    /// unpack_into(), of the key unpacked already: @k.
    pub(crate) fn unpacked_into(&self, f: &BkeyFormat, k: Bkey, dst: &mut [u64]) {
        let val = self.val(f);

        plain_bytes_mut(&mut dst[..BKEY_U64S]).copy_from_slice(plain_bytes(&k));
        dst[BKEY_U64S..][..val.len()].copy_from_slice(val);
    }

    pub fn unpack_pos(&self, b: &NodeContents) -> Bpos {
        if likely(self.is_packed()) {
            b.format.unpack_pos(self)
        } else {
            self.unpacked_header().p
        }
    }

    /// The key unpacked into @u, the value where it is.
    pub fn disassemble<'a>(&'a self, b: &NodeContents, u: &'a mut Bkey) -> BkeySC<'a> {
        self.disassembled(&b.format, self.unpack(b), u)
    }

    /// disassemble(), of the key unpacked already: @k.
    pub(crate) fn disassembled<'a>(&'a self, f: &BkeyFormat, k: Bkey,
                                   u: &'a mut Bkey) -> BkeySC<'a> {
        *u = k;
        BkeySC {
            k:    u,
            v:    self.val(f).as_ptr().cast(),
            iter: PhantomData,
            val:  PhantomData,
        }
    }
}

impl NodeContents {
    pub fn cmp_packed(&self, l: &BkeyP, r: &BkeyP) -> Ordering {
        self.cmp_packed_inlined(l, r)
    }

    /// For the sorts' merge loops.
    #[inline(always)]
    pub fn cmp_packed_inlined(&self, l: &BkeyP, r: &BkeyP) -> Ordering {
        if likely(l.is_packed() && r.is_packed()) {
            self.cmp_packed_format_checked(l, r)
        } else {
            self.cmp_mixed(l, r)
        }
    }

    #[cold]
    #[inline(never)]
    fn cmp_mixed(&self, l: &BkeyP, r: &BkeyP) -> Ordering {
        l.unpack_pos(self).cmp(&r.unpack_pos(self))
    }

    /// Both keys packed.
    #[inline(always)]
    pub fn cmp_packed_format_checked(&self, l: &BkeyP, r: &BkeyP) -> Ordering {
        let f = &self.format;

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(l.is_packed() && r.is_packed());
            assert!(self.nr_key_bits as u32 == f.key_bits());
        }
        invariant!(f.key_u64s != 0);

        let ret = f.cmp_bits(self.nr_key_bits as u32, l, r);

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(ret == l.unpack_pos(self).cmp(&r.unpack_pos(self)));
        }
        ret
    }

    pub fn cmp_left_packed(&self, l: &BkeyP, r: &Bpos) -> Ordering {
        l.unpack_pos(self).cmp(r)
    }
}

/*
 * It's safe to treat an unpacked bkey as a packed one, but not the reverse
 */
impl<'a, S: State> From<&'a AnyKey<S>> for &'a BkeyP<S> {
    fn from(k: &'a AnyKey<S>) -> Self {
        unsafe { &*<BkeyP<S> as Unsized>::from_raw(core::ptr::from_ref(k).cast(), k.k.bytes()) }
    }
}

impl<'a, S: State> From<&'a mut AnyKey<S>> for &'a mut BkeyP<S> {
    fn from(k: &'a mut AnyKey<S>) -> Self {
        let bytes = k.k.bytes();
        unsafe { &mut *<BkeyP<S> as Unsized>::from_raw_mut(core::ptr::from_mut(k).cast(), bytes) }
    }
}

/// Only a key that isn't packed is a bkey_i: the key back if it is.
impl<'a, S: State> TryFrom<&'a BkeyP<S>> for &'a AnyKey<S> {
    type Error = &'a BkeyP<S>;

    fn try_from(k: &'a BkeyP<S>) -> Result<Self, Self::Error> {
        if !k.is_bkey_i() {
            return Err(k);
        }
        Ok(unsafe { &*cast_unsized(core::ptr::from_ref(k).cast()) })
    }
}

impl<'a, S: State> TryFrom<&'a mut BkeyP<S>> for &'a mut AnyKey<S> {
    type Error = &'a mut BkeyP<S>;

    fn try_from(k: &'a mut BkeyP<S>) -> Result<Self, Self::Error> {
        if !k.is_bkey_i() {
            return Err(k);
        }
        Ok(unsafe { &mut *cast_unsized_mut(core::ptr::from_mut(k).cast()) })
    }
}

