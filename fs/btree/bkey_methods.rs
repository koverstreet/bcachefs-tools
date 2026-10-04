// SPDX-License-Identifier: GPL-2.0

//! Per-key-type methods, the Rust side of bkey_methods.c - so far, set():
//! assign a field of a key's value by name, from text.
//!
//! Reading is to_text(), the faithful rendering; set() is the write half, for
//! the debug tools and injection tests (kvdb). Fields are named and located by
//! typeinfo, and how depends on how the value is laid out:
//!
//!  - fixed layout: the field is written in place, in the value's bytes. A
//!    field past the end of a short value (an older format version) grows the
//!    value, zero-filled.
//!  - inodes: the varint-packed fields have no fixed position, so set() goes
//!    through the decoded form - bch2_inode_unpack(), assign in
//!    bch_inode_unpacked, bch2_inode_pack(). That writes inode_v3 whatever
//!    the key was, and can only produce what the packer produces:
//!    has_inode_opts, for one, is recomputed. The key stays where it is -
//!    bi_inum and bi_snapshot are just fields.
//!  - dirents: the target is a union tagged by d_type, and d_type is a C
//!    bitfield, so those fields are written by name - each one exactly, so
//!    the tag and the target can be made to disagree.
//!  - extents: not yet. An entry's position depends on the entries before
//!    it, and the entries are C bitfields, which typeinfo doesn't describe.
//!
//! A value is an integer (decimal, 0x hex, or negative), or for an
//! enum-coded field the codeword's name.

use crate::btree::bkey::{BkeyS, BkeySC};
use crate::btree::iter::TransBkey;
use crate::c;
use crate::fs::Fs;
use crate::inode;
use crate::snapshot_states::{SNAPSHOT_STATE_VALUES, SUBVOLUME_STATE_VALUES};
use crate::typeinfo::{self, AccessError, FieldTarget, ResolveError, TypeInfo};
use core::fmt;
use core::mem::{size_of, MaybeUninit};

const BKEY_U64S: usize = size_of::<c::bkey>() / size_of::<u64>();

type EnumValues = &'static [(&'static str, u64)];

pub enum SetError<'p> {
    /// No field by that name in this key type's value.
    Resolve(ResolveError<'p>),
    Access { field: &'p str, err: AccessError },
    BadValue { field: &'p str, val: &'p str, valid: Option<EnumValues> },
    /// The key's buffer can't hold the result.
    NoRoom { need_u64s: usize, have_u64s: usize },
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
            SetError::NoRoom { need_u64s, have_u64s } =>
                write!(f, "key needs {need_u64s} u64s, buffer has {have_u64s}"),
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

/// The fields that hold enum codewords. The name<->value tables come from the
/// x-macros (codegen.rs); this goes away when the format has a real schema.
fn field_enum(type_: u8, field: &str) -> Option<EnumValues> {
    use c::bch_bkey_type as t;

    match (type_ as u32, field) {
        (ty, "state") if ty == t::KEY_TYPE_snapshot.0  => Some(SNAPSHOT_STATE_VALUES),
        (ty, "state") if ty == t::KEY_TYPE_subvolume.0 => Some(SUBVOLUME_STATE_VALUES),
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

fn parse_val<'p>(type_: u8, field: &'p str, val: &'p str) -> Result<u64, SetError<'p>> {
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
    /// Set @field of this key's value to @val - see the notes at the top of
    /// this file. The key's buffer must have room for the result: a value
    /// that grows to reach the field, or the repacked inode.
    pub fn set<'p>(&mut self, fs: &Fs, field: &'p str, val: &'p str)
        -> Result<(), SetError<'p>>
    {
        if inode::bkey_is_inode(self.k()) {
            self.set_inode(fs, field, val)
        } else if self.k().type_ as u32 == c::bch_bkey_type::KEY_TYPE_dirent.0 {
            self.set_dirent(field, val)
        } else {
            self.set_fixed(field, val)
        }
    }

    /// The dirent's target is a union tagged by d_type - d_inum, or
    /// (d_child_subvol, d_parent_subvol) for DT_SUBVOL - and d_type is a C
    /// bitfield: typeinfo reaches neither. Each of these writes exactly the
    /// field named, tag or not, so a test can make them disagree.
    fn set_dirent<'p>(&mut self, field: &'p str, val: &'p str) -> Result<(), SetError<'p>> {
        let overflow = |bytes, val| SetError::Access {
            field,
            err: AccessError::Overflow { bytes, val },
        };

        if !matches!(field, "d_type" | "d_inum" | "d_child_subvol" | "d_parent_subvol") {
            return self.set_fixed(field, val);
        }

        let v = parse_val(self.k().type_, field, val)?;
        let d = unsafe { &mut *(&mut self.k_i_mut().v as *mut c::bch_val as *mut c::bch_dirent) };

        match field {
            "d_type" => {
                if v >= 1 << 5 {
                    return Err(SetError::Access {
                        field,
                        err: AccessError::BitsOverflow { bits: 5, val: v },
                    });
                }
                d.set_d_type(v as u8);
            }
            "d_inum" => d.__bindgen_anon_1.d_inum = v.to_le(),
            _ => {
                let v = u32::try_from(v).map_err(|_| overflow(4, v))?.to_le();
                let s = unsafe { &mut d.__bindgen_anon_1.__bindgen_anon_1 };
                if field == "d_child_subvol" {
                    s.d_child_subvol = v;
                } else {
                    s.d_parent_subvol = v;
                }
            }
        }
        Ok(())
    }

    fn set_fixed<'p>(&mut self, field: &'p str, val: &'p str) -> Result<(), SetError<'p>> {
        let type_ = self.k().type_;
        let info = typeinfo::bkey_val_info(type_ as u32)
            .ok_or(SetError::Unsupported { type_ })?;
        let target = typeinfo::resolve_with_bits(info, field)?;
        let v = parse_val(type_, field, val)?;

        let need_u64s = BKEY_U64S + (target.0.offset + target.0.len).div_ceil(8);
        let have_u64s = self.as_u64s().len();
        let u64s = self.k().u64s as usize;

        if u64s < need_u64s {
            if need_u64s > have_u64s {
                return Err(SetError::NoRoom { need_u64s, have_u64s });
            }
            self.as_mut_u64s()[u64s..need_u64s].fill(0);
            self.k_mut().u64s = need_u64s as u8;
        }

        let mut k = BkeyS::from(self.k_i_mut());
        write(k.val_bytes_mut(), field, &target, v)
    }

    fn set_inode<'p>(&mut self, fs: &Fs, field: &'p str, val: &'p str) -> Result<(), SetError<'p>> {
        let mut u = inode::unpack(fs, BkeySC::from(self.k_i()));

        let target = typeinfo::resolve_with_bits(
            <c::bch_inode_unpacked as TypeInfo>::INFO, field)?;
        let v = parse_val(self.k().type_, field, val)?;

        let u_bytes = unsafe {
            core::slice::from_raw_parts_mut(&mut u as *mut _ as *mut u8,
                                            size_of::<c::bch_inode_unpacked>())
        };
        write(u_bytes, field, &target, v)?;

        let mut packed: MaybeUninit<c::bkey_inode_buf> = MaybeUninit::zeroed();
        unsafe { c::bch2_inode_pack(fs.raw, packed.as_mut_ptr(), &u) };

        // bkey_inode_buf starts with the packed key, header first:
        let packed_u64s = unsafe {
            core::slice::from_raw_parts_mut(packed.as_mut_ptr() as *mut u64,
                                            size_of::<c::bkey_inode_buf>() / size_of::<u64>())
        };
        let packed_k = unsafe { &mut *(packed_u64s.as_mut_ptr() as *mut c::bkey) };
        packed_k.p = self.k().p;

        let need_u64s = packed_k.u64s as usize;
        let have_u64s = self.as_u64s().len();
        if need_u64s > have_u64s {
            return Err(SetError::NoRoom { need_u64s, have_u64s });
        }

        self.as_mut_u64s()[..need_u64s].copy_from_slice(&packed_u64s[..need_u64s]);
        Ok(())
    }
}
