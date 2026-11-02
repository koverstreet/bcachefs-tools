//! BkeyP, a key as a btree node holds it: mod.rs has how it fits with the
//! other forms.

use core::cmp::Ordering;
use core::marker::PhantomData;
use core::mem::MaybeUninit;

use super::{AllTypes, BkeyI, BkeyS, BkeySC, FamilyVal, ValidatedVal, BKEY_U64S};
use crate::btree::node::BtreeNode;
use crate::c;
use crate::format::Bpos;
use crate::util::kernel::likely;
use crate::util::vstructs::{plain_bytes, Unsized};

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
struct Header {
    u64s:        u8,
    format_bits: c::bkey_format_bits,
    type_:       u8,
}

impl BkeyP {
    pub unsafe fn from_raw(p: *const c::bkey_packed) -> *const Self {
        unsafe { core::ptr::slice_from_raw_parts(p.cast::<u64>(), (*p).u64s as usize) as *const Self }
    }

    pub unsafe fn from_raw_mut(p: *mut c::bkey_packed) -> *mut Self {
        unsafe { core::ptr::slice_from_raw_parts_mut(p.cast::<u64>(), (*p).u64s as usize) as *mut Self }
    }
}

impl<V: ?Sized> BkeyP<V> {
    pub fn as_ffi(&self) -> *const c::bkey_packed {
        (self as *const Self).cast()
    }

    pub fn as_ffi_mut(&mut self) -> *mut c::bkey_packed {
        (self as *mut Self).cast()
    }

    pub(super) fn cast<W: ?Sized>(&self) -> &BkeyP<W> {
        unsafe { &*(self as *const Self as *const BkeyP<W>) }
    }

    pub(super) fn cast_mut<W: ?Sized>(&mut self) -> &mut BkeyP<W> {
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

    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.header().type_ as u32)
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

    fn high_word(&self, f: &c::bkey_format) -> *const u64 {
        let w = self.data.as_ptr();
        if cfg!(target_endian = "little") { w.wrapping_add(f.key_u64s as usize - 1) } else { w }
    }

    fn unpacked_header(&self) -> &c::bkey {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(self.is_bkey_i());
        }
        unsafe { &*self.as_ffi().cast::<c::bkey>() }
    }

    fn unpack_format_checked(&self, b: &BtreeNode) -> c::bkey {
        let mut out = MaybeUninit::<c::bkey>::uninit();
        let out = unsafe {
            c::__bch2_bkey_unpack_key_b(b, out.as_mut_ptr(), self.as_ffi());
            out.assume_init()
        };

        if cfg!(CONFIG_BCACHEFS_DEBUG) && crate::debug::params::debug_check_bkey_unpack() {
            self.unpack_verify(b, &out);
        }
        out
    }

    #[cold]
    #[inline(never)]
    fn unpack_verify(&self, b: &BtreeNode, out: &c::bkey) {
        let mut check = MaybeUninit::<c::bkey>::uninit();
        let check = unsafe {
            c::__bch2_bkey_unpack_key(&b.format, check.as_mut_ptr(), self.as_ffi());
            check.assume_init()
        };
        assert!(plain_bytes(out) == plain_bytes(&check),
                "unpacked by its node's constants to {out}, by its format to {check}");
    }

    /// Just the key, not the value.
    pub fn unpack(&self, b: &BtreeNode) -> c::bkey {
        if likely(self.is_packed()) {
            self.unpack_format_checked(b)
        } else {
            *self.unpacked_header()
        }
    }

    pub fn unpack_pos(&self, b: &BtreeNode) -> Bpos {
        if likely(self.is_packed()) {
            unsafe { c::__bkey_unpack_pos_b(b, self.as_ffi()) }
        } else {
            self.unpacked_header().p
        }
    }

    /// The key unpacked into @u, the value where it is.
    pub fn disassemble<'a>(&'a self, b: &BtreeNode, u: &'a mut c::bkey) -> BkeySC<'a, V> {
        *u = self.unpack(b);
        BkeySC {
            k:    u,
            v:    self.val(&b.format).as_ptr().cast(),
            iter: PhantomData,
            val:  PhantomData,
        }
    }

    pub fn disassemble_mut<'a>(&'a mut self, b: &BtreeNode, u: &'a mut c::bkey) -> BkeyS<'a, V> {
        *u = self.unpack(b);
        BkeyS {
            k:   u,
            v:   self.val_mut(&b.format).as_mut_ptr().cast(),
            val: PhantomData,
        }
    }
}

impl BtreeNode {
    pub fn cmp_packed(&self,
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
    pub fn cmp_packed_format_checked(&self,
                                     l: &BkeyP<impl ?Sized + ValidatedVal>,
                                     r: &BkeyP<impl ?Sized + ValidatedVal>) -> Ordering {
        let f = &self.format;

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(l.is_packed() && r.is_packed());
            assert!(self.nr_key_bits as u32 == f.key_bits());
        }

        let ret = unsafe { cmp_bits(l.high_word(f), r.high_word(f), self.nr_key_bits as u32) };

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

impl c::bkey_format {
    /// The bits of a key's position.
    pub fn key_bits(&self) -> u32 {
        self.bits_per_field[c::bch_bkey_fields::BKEY_FIELD_INODE as usize] as u32 +
        self.bits_per_field[c::bch_bkey_fields::BKEY_FIELD_OFFSET as usize] as u32 +
        self.bits_per_field[c::bch_bkey_fields::BKEY_FIELD_SNAPSHOT as usize] as u32
    }
}

unsafe fn cmp_bits(mut l: *const u64, mut r: *const u64, mut nr_key_bits: u32) -> Ordering {
    const HIGH_BIT_OFFSET: u32 = if cfg!(target_endian = "little") { 0 } else { c::KEY_PACKED_BITS_START };

    let next_word = |p: *const u64| {
        if cfg!(target_endian = "little") { p.wrapping_sub(1) } else { p.wrapping_add(1) }
    };

    if nr_key_bits == 0 {
        return Ordering::Equal;
    }

    /* for big endian, skip past header */
    nr_key_bits += HIGH_BIT_OFFSET;
    let mut l_v = unsafe { *l } & (!0u64 >> HIGH_BIT_OFFSET);
    let mut r_v = unsafe { *r } & (!0u64 >> HIGH_BIT_OFFSET);

    loop {
        if nr_key_bits < 64 {
            l_v >>= 64 - nr_key_bits;
            r_v >>= 64 - nr_key_bits;
            nr_key_bits = 0;
        } else {
            nr_key_bits -= 64;
        }

        if nr_key_bits == 0 || l_v != r_v {
            break;
        }

        l = next_word(l);
        r = next_word(r);

        l_v = unsafe { *l };
        r_v = unsafe { *r };
    }

    l_v.cmp(&r_v)
}
