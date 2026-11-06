//! BkeyP, a key as a btree node holds it: mod.rs has how it fits with the
//! other forms.

use core::cmp::Ordering;
use core::marker::PhantomData;

use super::{AllTypes, BkeyI, BkeyS, BkeySC, FamilyVal, ValidatedVal, BKEY_U64S};
use crate::format::BkeyType;
use crate::btree::node::NodeContents;
use crate::c;
use crate::format::Bpos;
use crate::util::kernel::likely;
use crate::util::vstructs::{cast_plain_slice, plain_bytes, plain_bytes_mut, Unsized, Unvalidated};

/// V, as a BkeyI's, is the key's type and state - only a marker: where the
/// value starts is the node's format's.
#[repr(C)]
pub struct BkeyP<V: ?Sized = FamilyVal<AllTypes>> {
    val:  PhantomData<V>,
    data: [u64],
}

/// bkey_packed's first three bytes: bkey_packed itself is padded to a bkey's
/// size, so a reference to one reaches past a short key.
#[repr(C)]
pub(super) struct Header {
    pub(super) u64s:        u8,
    pub(super) format_bits: c::bkey_format_bits,
    pub(super) type_:       u8,
}

impl Header {
    pub(super) fn of(w: &u64) -> &Self {
        unsafe { &*(w as *const u64).cast() }
    }

    pub(super) fn of_mut(w: &mut u64) -> &mut Self {
        unsafe { &mut *(w as *mut u64).cast() }
    }
}

impl BkeyP {
    pub unsafe fn from_raw(p: *const c::bkey_packed) -> *const Self {
        unsafe { core::ptr::slice_from_raw_parts(p.cast::<u64>(), (*p).u64s as usize) as *const Self }
    }

    pub unsafe fn from_raw_mut(p: *mut c::bkey_packed) -> *mut Self {
        unsafe { core::ptr::slice_from_raw_parts_mut(p.cast::<u64>(), (*p).u64s as usize) as *mut Self }
    }

    /// The key @s starts with, if all of it is in @s.
    pub fn from_slice(s: &[u64]) -> Option<&BkeyP<FamilyVal<AllTypes, Unvalidated>>> {
        let u64s = s.first()?.to_ne_bytes()[0] as usize;
        if u64s == 0 || u64s > s.len() {
            return None;
        }
        let s = &s[..u64s];
        Some(unsafe { &*(s as *const [u64] as *const BkeyP<FamilyVal<AllTypes, Unvalidated>>) })
    }

    /// A key in a bkey_packed of its own, as one packed onto the stack.
    pub fn from_packed(k: &c::bkey_packed) -> Option<&BkeyP<FamilyVal<AllTypes, Unvalidated>>> {
        Self::from_slice(cast_plain_slice(plain_bytes(k)).expect("a bkey_packed is whole u64s"))
    }

    pub fn from_slice_mut(s: &mut [u64]) -> Option<&mut BkeyP<FamilyVal<AllTypes, Unvalidated>>> {
        let u64s = s.first()?.to_ne_bytes()[0] as usize;
        let s = s.get_mut(..u64s).filter(|s| !s.is_empty())?;
        Some(unsafe { &mut *(s as *mut [u64] as *mut BkeyP<FamilyVal<AllTypes, Unvalidated>>) })
    }
}

impl<V: ?Sized> BkeyP<V> {
    pub fn as_ffi(&self) -> *const c::bkey_packed {
        (self as *const Self).cast()
    }

    pub fn as_ffi_mut(&mut self) -> *mut c::bkey_packed {
        (self as *mut Self).cast()
    }

    pub fn as_slice(&self) -> &[u64] {
        &self.data
    }

    pub(crate) fn cast<W: ?Sized>(&self) -> &BkeyP<W> {
        unsafe { &*(self as *const Self as *const BkeyP<W>) }
    }

    pub(crate) fn cast_mut<W: ?Sized>(&mut self) -> &mut BkeyP<W> {
        unsafe { &mut *(self as *mut Self as *mut BkeyP<W>) }
    }

    fn header(&self) -> &Header {
        unsafe { &*self.as_ffi().cast::<Header>() }
    }

    fn header_mut(&mut self) -> &mut Header {
        unsafe { &mut *self.as_ffi_mut().cast::<Header>() }
    }

    pub fn u64s(&self) -> usize {
        self.header().u64s as usize
    }

    pub fn bytes(&self) -> usize {
        self.u64s() * size_of::<u64>()
    }

    pub fn format(&self) -> u8 {
        self.header().format_bits.format()
    }

    pub fn is_packed(&self) -> bool {
        let format = self.format() as u32;
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(format <= c::KEY_FORMAT_CURRENT, "key of format {format}");
        }
        format != c::KEY_FORMAT_CURRENT
    }

    pub fn needs_whiteout(&self) -> bool {
        self.header().format_bits.needs_whiteout() != 0
    }

    pub fn set_needs_whiteout(&mut self, v: bool) {
        self.header_mut().format_bits.set_needs_whiteout(v as u8);
    }

    pub fn set_format(&mut self, format: u8) {
        self.header_mut().format_bits.set_format(format);
    }

    pub fn key_type(&self) -> BkeyType {
        BkeyType(self.header().type_ as u32)
    }

    pub fn key_u64s(&self, f: &c::bkey_format) -> usize {
        if self.is_packed() { f.key_u64s as usize } else { BKEY_U64S }
    }

    pub fn key_bytes(&self, f: &c::bkey_format) -> usize {
        self.key_u64s(f) * size_of::<u64>()
    }

    /// Its value fits a bkey_i's, once unpacked.
    pub fn u64s_valid(&self, f: &c::bkey_format) -> bool {
        self.u64s()
            .checked_sub(self.key_u64s(f))
            .is_some_and(|v| v <= u8::MAX as usize - BKEY_U64S)
    }

    fn is_bkey_i(&self) -> bool {
        !self.is_packed() && self.u64s() >= BKEY_U64S
    }
}

/// Reading past the header - the value, or an unpacked key's whole header,
/// a bkey - needs the key's u64s to be validate's.
impl<V: ?Sized + ValidatedVal> BkeyP<V> {
    pub fn val_u64s(&self, f: &c::bkey_format) -> usize {
        self.u64s() - self.key_u64s(f)
    }

    pub fn val_bytes(&self, f: &c::bkey_format) -> usize {
        self.val_u64s(f) * size_of::<u64>()
    }

    pub fn val(&self, f: &c::bkey_format) -> &[u64] {
        let key_u64s = self.key_u64s(f);
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(key_u64s <= self.data.len());
        }
        unsafe { self.data.get_unchecked(key_u64s..) }
    }

    pub fn val_mut(&mut self, f: &c::bkey_format) -> &mut [u64] {
        let key_u64s = self.key_u64s(f);
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(key_u64s <= self.data.len());
        }
        unsafe { self.data.get_unchecked_mut(key_u64s..) }
    }

    fn unpacked_header(&self) -> &c::bkey {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(self.is_bkey_i());
        }
        unsafe { &*self.as_ffi().cast::<c::bkey>() }
    }

    /// Just the key, not the value.
    pub fn unpack(&self, b: &NodeContents) -> c::bkey {
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
    pub(crate) fn unpacked_into(&self, f: &c::bkey_format, k: c::bkey, dst: &mut [u64]) {
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
    pub fn disassemble<'a>(&'a self, b: &NodeContents, u: &'a mut c::bkey) -> BkeySC<'a, V> {
        self.disassembled(&b.format, self.unpack(b), u)
    }

    /// disassemble(), of the key unpacked already: @k.
    pub(crate) fn disassembled<'a>(&'a self, f: &c::bkey_format, k: c::bkey,
                                   u: &'a mut c::bkey) -> BkeySC<'a, V> {
        *u = k;
        BkeySC {
            k:    u,
            v:    self.val(f).as_ptr().cast(),
            iter: PhantomData,
            val:  PhantomData,
        }
    }

    pub fn disassemble_mut<'a>(&'a mut self, b: &NodeContents, u: &'a mut c::bkey) -> BkeyS<'a, V> {
        *u = self.unpack(b);
        BkeyS {
            k:   u,
            v:   self.val_mut(&b.format).as_mut_ptr().cast(),
            val: PhantomData,
        }
    }
}

impl NodeContents {
    pub fn cmp_packed(&self,
                      l: &BkeyP<impl ?Sized + ValidatedVal>,
                      r: &BkeyP<impl ?Sized + ValidatedVal>) -> Ordering {
        self.cmp_packed_inlined(l, r)
    }

    /// For the sorts' merge loops.
    #[inline(always)]
    pub fn cmp_packed_inlined(&self,
                              l: &BkeyP<impl ?Sized + ValidatedVal>,
                              r: &BkeyP<impl ?Sized + ValidatedVal>) -> Ordering {
        if likely(l.is_packed() && r.is_packed()) {
            self.cmp_packed_format_checked(l, r)
        } else {
            self.cmp_mixed(l, r)
        }
    }

    #[cold]
    #[inline(never)]
    fn cmp_mixed(&self,
                 l: &BkeyP<impl ?Sized + ValidatedVal>,
                 r: &BkeyP<impl ?Sized + ValidatedVal>) -> Ordering {
        l.unpack_pos(self).cmp(&r.unpack_pos(self))
    }

    /// Both keys packed.
    #[inline(always)]
    pub fn cmp_packed_format_checked(&self,
                                     l: &BkeyP<impl ?Sized + ValidatedVal>,
                                     r: &BkeyP<impl ?Sized + ValidatedVal>) -> Ordering {
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

    pub fn cmp_left_packed(&self, l: &BkeyP<impl ?Sized + ValidatedVal>, r: &Bpos) -> Ordering {
        l.unpack_pos(self).cmp(r)
    }
}

/*
 * It's safe to treat an unpacked bkey as a packed one, but not the reverse
 */
impl<'a, V: ?Sized> From<&'a BkeyI<V>> for &'a BkeyP<V> {
    fn from(k: &'a BkeyI<V>) -> Self {
        let u64s = k.k.u64s as usize;
        unsafe { &*(core::ptr::slice_from_raw_parts(k.as_ffi().cast::<u64>(), u64s) as *const BkeyP<V>) }
    }
}

impl<'a, V: ?Sized> From<&'a mut BkeyI<V>> for &'a mut BkeyP<V> {
    fn from(k: &'a mut BkeyI<V>) -> Self {
        let u64s = k.k.u64s as usize;
        unsafe { &mut *(core::ptr::slice_from_raw_parts_mut(k.as_ffi_mut().cast::<u64>(), u64s) as *mut BkeyP<V>) }
    }
}

/// Only a key that isn't packed is a bkey_i: the key back if it is.
impl<'a, V: ?Sized + Unsized> TryFrom<&'a BkeyP<V>> for &'a BkeyI<V> {
    type Error = &'a BkeyP<V>;

    fn try_from(k: &'a BkeyP<V>) -> Result<Self, Self::Error> {
        if !k.is_bkey_i() {
            return Err(k);
        }
        Ok(unsafe { &*super::types::cast_unsized(k.as_ffi().cast()) })
    }
}

impl<'a, V: ?Sized + Unsized> TryFrom<&'a mut BkeyP<V>> for &'a mut BkeyI<V> {
    type Error = &'a mut BkeyP<V>;

    fn try_from(k: &'a mut BkeyP<V>) -> Result<Self, Self::Error> {
        if !k.is_bkey_i() {
            return Err(k);
        }
        Ok(unsafe { &mut *super::types::cast_unsized_mut(k.as_ffi_mut().cast()) })
    }
}

