#![allow(non_camel_case_types)]

use super::iter::BtreeIter;
use crate::c;
#[cfg(feature = "std")]
use crate::fs::Fs;
#[cfg(feature = "std")]
use crate::printbuf_to_formatter;
#[cfg(feature = "std")]
use core::fmt;
#[cfg(feature = "std")]
use core::str::FromStr;
#[cfg(feature = "std")]
use crate::errcode::{bch_errcode, BchError};
use core::marker::PhantomData;

use c::bpos as Bpos;

use core::cmp::Ordering;

pub struct Bkey<K: BkeyInit> {
    raw: K,
}

pub trait AsBkeyI {
    fn as_bkey_i(&self) -> &c::bkey_i;
    fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i;

    fn as_bkey(&self) -> &c::bkey {
        &self.as_bkey_i().k
    }

    fn as_bkey_mut(&mut self) -> &mut c::bkey {
        &mut self.as_bkey_i_mut().k
    }
}

impl<K: BkeyInit> Bkey<K> {
    pub fn new() -> Self {
        let mut raw = K::default();
        raw.init();
        Self { raw }
    }

    pub fn raw(&self) -> &K {
        &self.raw
    }

    pub fn raw_mut(&mut self) -> &mut K {
        &mut self.raw
    }

    pub fn k(&self) -> &c::bkey {
        self.raw.k()
    }

    pub fn k_mut(&mut self) -> &mut c::bkey {
        self.raw.k_mut()
    }

    pub fn k_i(&self) -> &c::bkey_i {
        self.raw.k_i()
    }

    pub fn k_i_mut(&mut self) -> &mut c::bkey_i {
        self.raw.k_i_mut()
    }
}

impl<K: BkeyInit> AsBkeyI for Bkey<K> {
    fn as_bkey_i(&self) -> &c::bkey_i {
        self.k_i()
    }

    fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i {
        self.k_i_mut()
    }
}

impl AsBkeyI for c::bkey_i {
    fn as_bkey_i(&self) -> &c::bkey_i {
        self
    }

    fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i {
        self
    }
}

impl<K: BkeyInit> Default for Bkey<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl c::bkey {
    pub fn pos(&self) -> c::bpos {
        self.p
    }

    pub fn set_pos(&mut self, pos: c::bpos) {
        self.p = pos;
    }

    pub fn set_snapshot(&mut self, snapshot: u32) {
        self.p.snapshot = snapshot;
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn set_size(&mut self, size: u32) {
        self.size = size;
    }

    pub fn set_range(&mut self, inode: u64, start: u64, end: u64, snapshot: u32) {
        self.p = spos(inode, end, snapshot);
        self.size = (end - start) as u32;
    }

    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.type_ as u32)
    }

    pub fn set_version_lo(&mut self, version: u64) {
        self.bversion.lo = version;
    }

    pub fn start_pos(&self) -> c::bpos {
        bkey_start_pos(self)
    }

    pub fn start_offset(&self) -> u64 {
        bkey_start_offset(self)
    }

    pub fn is_deleted(&self) -> bool {
        bkey_deleted(self)
    }

    pub fn is_btree_ptr(&self) -> bool {
        bkey_is_btree_ptr(self)
    }
}

impl PartialEq for Bpos {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Bpos {}

impl PartialOrd for Bpos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bpos {
    fn cmp(&self, other: &Self) -> Ordering {
        let l_inode = self.inode;
        let r_inode = other.inode;
        let l_offset = self.offset;
        let r_offset = other.offset;
        let l_snapshot = self.snapshot;
        let r_snapshot = other.snapshot;

        l_inode
            .cmp(&r_inode)
            .then(l_offset.cmp(&r_offset))
            .then(l_snapshot.cmp(&r_snapshot))
    }
}

pub const fn spos(inode: u64, offset: u64, snapshot: u32) -> Bpos {
    Bpos {
        inode,
        offset,
        snapshot,
    }
}

pub const fn pos(inode: u64, offset: u64) -> Bpos {
    spos(inode, offset, 0)
}

pub const POS_MIN: Bpos = spos(0, 0, 0);
pub const POS_MAX: Bpos = spos(u64::MAX, u64::MAX, 0);
pub const SPOS_MAX: Bpos = spos(u64::MAX, u64::MAX, u32::MAX);

/// Parse a bpos field that holds a u64 (inode, offset). Accepts the literal
/// tokens "U64_MAX" / "U32_MAX" as their respective sentinel values, matching
/// how positions are printed in dmesg / bcachefs_to_text output.
#[cfg(feature = "std")]
fn parse_bpos_u64(s: &str) -> Result<u64, BchError> {
    match s {
        "U64_MAX" => Ok(u64::MAX),
        "U32_MAX" => Ok(u32::MAX as u64),
        _        => s.parse().map_err(|_| BchError::from_errcode(bch_errcode::BCH_ERR_EINVAL_parse_bpos)),
    }
}

/// Same for the snapshot field (u32).
#[cfg(feature = "std")]
fn parse_bpos_u32(s: &str) -> Result<u32, BchError> {
    match s {
        "U32_MAX" => Ok(u32::MAX),
        _        => s.parse().map_err(|_| BchError::from_errcode(bch_errcode::BCH_ERR_EINVAL_parse_bpos)),
    }
}

#[cfg(feature = "std")]
impl FromStr for c::bpos {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "POS_MIN" {
            return Ok(POS_MIN);
        }

        if s == "POS_MAX" {
            return Ok(POS_MAX);
        }

        if s == "SPOS_MAX" {
            return Ok(SPOS_MAX);
        }

        let err = || BchError::from_errcode(bch_errcode::BCH_ERR_EINVAL_parse_bpos);

        let mut fields = s.split(':');
        let ino_str = fields.next().ok_or_else(err)?;
        let off_str = fields.next().ok_or_else(err)?;
        let snp_str = fields.next();

        let ino: u64 = parse_bpos_u64(ino_str)?;
        let off: u64 = parse_bpos_u64(off_str)?;
        let snp: u32 = snp_str
            .map(parse_bpos_u32)
            .transpose()?
            .unwrap_or(0);

        Ok(c::bpos {
            inode:    ino,
            offset:   off,
            snapshot: snp,
        })
    }
}

pub type bkey_type = c::bch_bkey_type;

#[derive(Clone, Copy)]
pub struct BkeySC<'a> {
    pub k:           &'a c::bkey,
    pub v:           &'a c::bch_val,
    pub(crate) iter: PhantomData<&'a mut BtreeIter<'a>>,
}

// Typed bkey dispatch enums — generated from BCH_BKEY_TYPES() x-macro
include!(concat!(env!("OUT_DIR"), "/bkey_types_gen.rs"));

impl<'a> BkeySC<'a> {
    #[cfg(feature = "std")]
    unsafe fn to_raw(&self) -> c::bkey_s_c {
        c::bkey_s_c {
            k: self.k,
            v: self.v,
        }
    }

    #[cfg(feature = "std")]
    pub fn to_text<'f>(&self, fs: &'f Fs) -> BkeySCToText<'a, 'f> {
        BkeySCToText {
            k: BkeySC { k: self.k, v: self.v, iter: PhantomData },
            fs,
        }
    }

    /// Key only - type, pos, size - without rendering the value.
    #[cfg(feature = "std")]
    pub fn to_text_key(&self) -> BkeySCKeyToText<'a> {
        BkeySCKeyToText {
            k: BkeySC { k: self.k, v: self.v, iter: PhantomData },
        }
    }

    pub fn v(&self) -> BkeyValSC<'a> {
        unsafe { BkeyValSC::from_raw(self.k, self.v) }
    }

    pub fn pos(&self) -> c::bpos {
        self.k.p
    }

    pub fn size(&self) -> u32 {
        self.k.size
    }

    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.k.type_ as u32)
    }

    pub fn is_deleted(&self) -> bool {
        self.key_type() == c::bch_bkey_type::KEY_TYPE_deleted
    }

    pub fn is_btree_ptr(&self) -> bool {
        matches!(
            self.key_type(),
            c::bch_bkey_type::KEY_TYPE_btree_ptr | c::bch_bkey_type::KEY_TYPE_btree_ptr_v2
        )
    }

    pub fn start_pos(&self) -> c::bpos {
        bkey_start_pos(self.k)
    }

    pub fn start_offset(&self) -> u64 {
        bkey_start_offset(self.k)
    }
}

impl<'a> From<&'a c::bkey_i> for BkeySC<'a> {
    fn from(k: &'a c::bkey_i) -> Self {
        BkeySC {
            k:    &k.k,
            v:    &k.v,
            iter: PhantomData,
        }
    }
}

impl<'a> From<&'a c::bkey_s_c> for BkeySC<'a> {
    fn from(k: &'a c::bkey_s_c) -> Self {
        BkeySC {
            k:    unsafe { &*k.k },
            v:    unsafe { &*k.v },
            iter: PhantomData,
        }
    }
}

/// Mutable counterpart of [`BkeySC`]: a borrowed, unpacked key + value with the
/// lifetime tracked, so mutations land on the underlying buffer. It is a mutable
/// handle, so it is deliberately not `Copy`; the extent iterators borrow it
/// (`&mut BkeyS`), which lets a single handle be iterated more than once (e.g. a
/// read scan then a rewrite pass) without ever aliasing `&mut`.
pub struct BkeyS<'a> {
    pub k: &'a mut c::bkey,
    pub v: &'a mut c::bch_val,
}

impl<'a> BkeyS<'a> {
    /// The raw `c::bkey_s` for passing to C helpers (e.g. `bch2_bkey_ptrs`).
    pub(crate) fn to_raw(&mut self) -> c::bkey_s {
        c::bkey_s {
            __bindgen_anon_1: c::bkey_s__bindgen_ty_1 {
                __bindgen_anon_1: c::bkey_s__bindgen_ty_1__bindgen_ty_1 {
                    k: &mut *self.k,
                    v: &mut *self.v,
                },
            },
        }
    }

    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.k.type_ as u32)
    }

    /// The value bytes, mutably. Length is derived from the key's `u64s`; the
    /// value is never packed, so this is the on-disk value region in place.
    pub fn val_bytes_mut(&mut self) -> &mut [u8] {
        let len = self.k.u64s as usize * 8 - core::mem::size_of::<c::bkey>();
        unsafe { core::slice::from_raw_parts_mut(self.v as *mut c::bch_val as *mut u8, len) }
    }
}

impl<'a> From<&'a mut c::bkey_i> for BkeyS<'a> {
    fn from(k: &'a mut c::bkey_i) -> Self {
        BkeyS {
            k: &mut k.k,
            v: &mut k.v,
        }
    }
}

#[cfg(feature = "std")]
pub struct BkeySCToText<'a, 'f> {
    k:  BkeySC<'a>,
    fs: &'f Fs,
}

#[cfg(feature = "std")]
impl fmt::Display for BkeySCToText<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        unsafe {
            printbuf_to_formatter(f, |buf| {
                c::bch2_bkey_val_to_text(buf, self.fs.raw, self.k.to_raw())
            })
        }
    }
}

#[cfg(feature = "std")]
pub struct BkeySCKeyToText<'a> {
    k: BkeySC<'a>,
}

#[cfg(feature = "std")]
impl fmt::Display for BkeySCKeyToText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        unsafe { printbuf_to_formatter(f, |buf| c::bch2_bkey_to_text(buf, self.k.k)) }
    }
}

#[inline(always)]
pub fn bpos_lt(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else {
        l.snapshot < r.snapshot
    }
}

#[inline(always)]
pub fn bpos_le(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else {
        l.snapshot <= r.snapshot
    }
}

#[inline(always)]
pub fn bpos_gt(l: Bpos, r: Bpos) -> bool {
    bpos_lt(r, l)
}

#[inline(always)]
pub fn bpos_ge(l: Bpos, r: Bpos) -> bool {
    bpos_le(r, l)
}

#[inline(always)]
pub fn bpos_cmp(l: Bpos, r: Bpos) -> i32 {
    if l.inode != r.inode {
        if l.inode < r.inode { -1 } else { 1 }
    } else if l.offset != r.offset {
        if l.offset < r.offset { -1 } else { 1 }
    } else if l.snapshot != r.snapshot {
        if l.snapshot < r.snapshot { -1 } else { 1 }
    } else {
        0
    }
}

#[inline]
pub fn bpos_min(l: Bpos, r: Bpos) -> Bpos {
    if bpos_lt(l, r) { l } else { r }
}

#[inline]
pub fn bpos_max(l: Bpos, r: Bpos) -> Bpos {
    if bpos_gt(l, r) { l } else { r }
}

#[inline(always)]
pub fn bkey_eq(l: Bpos, r: Bpos) -> bool {
    l.inode == r.inode && l.offset == r.offset
}

#[inline(always)]
pub fn bkey_lt(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset < r.offset
    }
}

#[inline(always)]
pub fn bkey_le(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset <= r.offset
    }
}

#[inline(always)]
pub fn bkey_gt(l: Bpos, r: Bpos) -> bool {
    bkey_lt(r, l)
}

#[inline(always)]
pub fn bkey_ge(l: Bpos, r: Bpos) -> bool {
    bkey_le(r, l)
}

#[inline(always)]
pub fn bkey_cmp(l: Bpos, r: Bpos) -> i32 {
    if l.inode != r.inode {
        if l.inode < r.inode { -1 } else { 1 }
    } else if l.offset != r.offset {
        if l.offset < r.offset { -1 } else { 1 }
    } else {
        0
    }
}

/// Start position of a bkey (p.offset - size).
pub fn bkey_start_pos(k: &c::bkey) -> c::bpos {
    c::bpos {
        inode: k.p.inode,
        offset: k.p.offset.wrapping_sub(k.size as u64),
        snapshot: k.p.snapshot,
    }
}

pub fn bkey_start_offset(k: &c::bkey) -> u64 {
    k.p.offset.wrapping_sub(k.size as u64)
}

pub fn bkey_deleted(k: &c::bkey) -> bool {
    c::bch_bkey_type(k.type_ as u32) == c::bch_bkey_type::KEY_TYPE_deleted
}

pub fn bkey_is_btree_ptr(k: &c::bkey) -> bool {
    matches!(
        c::bch_bkey_type(k.type_ as u32),
        c::bch_bkey_type::KEY_TYPE_btree_ptr | c::bch_bkey_type::KEY_TYPE_btree_ptr_v2
    )
}
