// SPDX-License-Identifier: GPL-2.0

//! set(): assign a field of a key's value by name, from text.
//!
//! Reading is to_text(), the faithful rendering; set() is the write half, for
//! the debug tools and injection tests (kvdb). Fields are named and located by
//! typeinfo, and how depends on how the value is laid out:
//!
//!  - fixed layout: the field is written in place, in the value's bytes. A
//!    field past the end of a short value (an older format version) grows the
//!    value, zero-filled.
//!  - inodes: the varint-packed fields have no fixed position, so set() goes
//!    through the decoded form - inode::unpack(), assign in
//!    bch_inode_unpacked, inode::pack(). That writes inode_v3 whatever
//!    the key was, and can only produce what the packer produces:
//!    has_inode_opts, for one, is recomputed. The key stays where it is -
//!    bi_inum and bi_snapshot are just fields.
//!  - dirents: the target, d_type and the name aren't fields typeinfo can
//!    reach - dirent::set_field().
//!  - pointers, of any key with them: an entry's position depends on the
//!    entries before it, and the entries are C bitfields, which typeinfo
//!    doesn't describe - so ptrs[N].field, N counting only the pointers,
//!    found by the entry walk and set in place: bch_extent_ptr::set_field().
//!    The other entries - crcs, an extent's stripe_ptr - can't be set yet.
//!
//! A value is an integer (decimal, 0x hex, or negative), or for an
//! enum-coded field the codeword's name.

#![allow(non_camel_case_types)]

use crate::btree::bkey::{BkeyS, BkeySC};
use crate::btree::bkey::prelude::*;
use crate::btree::iter::{NoRoom, TransBkey};
use crate::c;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::format::{BkeyType, ErrorKeyReason, MetadataVersion};
use crate::fs::Fs;
use crate::{dirent, inode};
use crate::typeinfo::{self, AccessError, FieldTarget, ResolveError, TypeInfo};
use crate::util::names::Names;
use core::fmt;
use core::mem::size_of;

type EnumValues = &'static [(&'static str, u64)];

pub enum SetError<'p> {
    Resolve(ResolveError<'p>),
    Access { field: &'p str, err: AccessError },
    BadValue { field: &'p str, val: &'p str, valid: Option<EnumValues> },
    NoRoom(NoRoom),
    /// A dirent name that isn't one: empty, or past BCH_NAME_MAX.
    BadName { val: &'p str },
    Unsupported { type_: u8 },
}

impl fmt::Display for SetError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetError::Resolve(e) => write!(f, "{e}"),
            SetError::Access { field, err } => write!(f, "{field}: {err}"),
            SetError::BadValue { field, val, valid: None } =>
                write!(f, "{field}: expected integer, got '{val}'"),
            SetError::BadValue { field, val, valid: Some(valid) } => {
                write!(f, "{field}: unknown value '{val}' (valid:")?;
                for (name, _) in *valid {
                    write!(f, " {name}")?;
                }
                write!(f, ")")
            }
            SetError::NoRoom(e) => write!(f, "{e}"),
            SetError::BadName { val } =>
                write!(f, "d_name: '{val}' isn't a valid dirent name"),
            SetError::Unsupported { type_ } =>
                write!(f, "setting fields of key type {type_} isn't supported"),
        }
    }
}

impl<'p> From<ResolveError<'p>> for SetError<'p> {
    fn from(e: ResolveError<'p>) -> Self {
        SetError::Resolve(e)
    }
}

impl From<NoRoom> for SetError<'_> {
    fn from(e: NoRoom) -> Self {
        SetError::NoRoom(e)
    }
}

/// The fields that hold enum codewords, and the enums' names; this goes away
/// when the format has a real schema.
fn field_enum(type_: u8, field: &str) -> Option<EnumValues> {
    match (type_ as u32, field) {
        (ty, "state") if ty == BkeyType::snapshot.0  => Some(<c::SnapshotState as Names>::ENTRIES),
        (ty, "state") if ty == BkeyType::subvolume.0 => Some(<c::SubvolumeState as Names>::ENTRIES),
        _ => None,
    }
}

/// An integer: decimal, 0x hex, or negative (two's complement).
pub fn parse_int(s: &str) -> Option<u64> {
    if let Some(h) = s.strip_prefix("0x") {
        u64::from_str_radix(h, 16).ok()
    } else if s.starts_with('-') {
        s.parse::<i64>().ok().map(|v| v as u64)
    } else {
        s.parse::<u64>().ok()
    }
}

pub(crate) fn parse_val<'p>(type_: u8, field: &'p str, val: &'p str) -> Result<u64, SetError<'p>> {
    if let Some(v) = parse_int(val) {
        return Ok(v);
    }

    let valid = field_enum(type_, field);
    valid.and_then(|vals| vals.iter().find(|(name, _)| *name == val))
        .map(|(_, v)| *v)
        .ok_or(SetError::BadValue { field, val, valid })
}

fn write<'p>(buf: &mut [u8], field: &'p str, target: &FieldTarget, v: u64)
    -> Result<(), SetError<'p>>
{
    typeinfo::write(buf, target, v).map_err(|err| SetError::Access { field, err })
}

impl TransBkey<'_, '_> {
    /// Made an error key, saying @err if the filesystem's version allows it:
    /// bch2_set_bkey_error().
    pub fn set_error(&mut self, fs: &Fs, err: ErrorKeyReason) {
        assert!(self.as_u64s().len() * 8 >= size_of::<c::bkey>() + size_of::<c::Error>(),
                "no room in the key's buffer for an error key");
        self.k_mut().set_key_type(BkeyType::error);

        if fs.request_incompat_feature(
            MetadataVersion::extented_key_type_error).is_ok() {
            self.k_mut().set_val_bytes(size_of::<c::Error>());
            let v = self.val_bytes_mut();
            v.fill(0);
            v[core::mem::offset_of!(c::Error, repr.err)] = err.0 as u8;
        } else {
            self.k_mut().set_val_bytes(0);
        }
    }

    /// The key's buffer must have room for the result: a value grown to reach
    /// the field, or the repacked inode.
    pub fn set<'p>(&mut self, fs: &Fs, field: &'p str, val: &'p str)
        -> Result<(), SetError<'p>>
    {
        if field.starts_with("ptrs[") {
            self.set_ptr_field(fs, field, val)
        } else if inode::bkey_is_inode(self.k()) {
            self.set_inode(fs, field, val)
        } else if self.k().type_ as u32 == BkeyType::dirent.0 {
            dirent::set_field(self, fs, field, val)
        } else {
            self.set_fixed(field, val)
        }
    }

    /// By the stored struct's field names: an inode isn't repacked, so what the
    /// packer recomputes (has_inode_opts) can be set wrong. For an inode, only
    /// bch_inode_v3's fixed fields.
    pub fn set_raw<'p>(&mut self, fs: &Fs, field: &'p str, val: &'p str) -> Result<(), SetError<'p>> {
        if field.starts_with("ptrs[") {
            self.set_ptr_field(fs, field, val)
        } else {
            self.set_fixed(field, val)
        }
    }

    /// ptrs[N].field: pointer N's, in place.
    fn set_ptr_field<'p>(&mut self, fs: &Fs, field: &'p str, val: &'p str) -> Result<(), SetError<'p>> {
        let type_ = self.k().type_;
        let (idx, sub) = field.strip_prefix("ptrs[").and_then(|f| f.split_once("]."))
            .ok_or(ResolveError::BadSyntax { at: field })?;
        let idx: usize = idx.parse().map_err(|_| ResolveError::BadSyntax { at: idx })?;
        let v = parse_val(type_, sub, val)?;

        let mut k = self.reborrow().arm::<Ptrs>().ok_or(SetError::Unsupported { type_ })?;
        let n = k.ptrs(fs).count();
        let ptr = k.ptrs_mut(fs).nth(idx).ok_or(ResolveError::IndexOutOfBounds { field: "ptrs", idx, n })?;
        ptr.set_field(sub, v)
    }

    /// In place; also dirent::set_field()'s fallback.
    pub(crate) fn set_fixed<'p>(&mut self, field: &'p str, val: &'p str) -> Result<(), SetError<'p>> {
        let type_ = self.k().type_;
        let info = self.k().key_type().info().ok_or(SetError::Unsupported { type_ })?;
        let target = typeinfo::resolve_with_bits(info, field)?;
        let v = parse_val(type_, field, val)?;

        // grown to reach the field, if it doesn't
        let need_val_u64s = (target.0.offset + target.0.len).div_ceil(8);
        if self.k().val_bytes() < need_val_u64s * size_of::<u64>() {
            self.set_val_u64s(need_val_u64s)?;
        }

        let mut k = BkeyS::from(&mut *self);
        write(k.val_bytes_mut(), field, &target, v)
    }

    fn set_inode<'p>(&mut self, fs: &Fs, field: &'p str, val: &'p str) -> Result<(), SetError<'p>> {
        let mut u = inode::unpack(fs, BkeySC::from(&*self));

        let target = typeinfo::resolve_with_bits(
            <c::bch_inode_unpacked as TypeInfo>::INFO, field)?;
        let v = parse_val(self.k().type_, field, val)?;

        let u_bytes = unsafe {
            core::slice::from_raw_parts_mut(&mut u as *mut _ as *mut u8,
                                            size_of::<c::bch_inode_unpacked>())
        };
        write(u_bytes, field, &target, v)?;

        let mut packed = inode::pack(&u);
        packed.inode.k.p = self.k().p;

        let need_u64s = packed.inode.k.u64s as usize;
        let have_u64s = self.as_u64s().len();
        if need_u64s > have_u64s {
            return Err(NoRoom { need_u64s, have_u64s }.into());
        }

        // The packed key, header first - within the buffer pack() sized for it:
        let packed_u64s = unsafe {
            core::slice::from_raw_parts(packed.as_ref().as_ffi().cast::<u64>(),
                                        need_u64s)
        };
        self.as_mut_u64s()[..need_u64s].copy_from_slice(packed_u64s);
        Ok(())
    }
}
