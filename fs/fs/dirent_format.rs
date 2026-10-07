// SPDX-License-Identifier: GPL-2.0

//! A dirent's value (fs/dirent_format.h): reading it, and for kvdb, writing
//! its fields.
//!
//! The value is two tagged unions. The target is an inode number, or for
//! d_type DT_SUBVOL a (child, parent) subvolume pair. The name block is the
//! name alone, its length implied by the value's size and NUL padding - or,
//! with d_casefold set, explicit lengths and the name followed by its
//! casefolded form, which is what the dirent is hashed and looked up by.
//!
//! Trust: nothing in a dirent value is trusted until it has passed validate,
//! and nothing but validate and to_text sees one that hasn't - keys read from
//! the btree were validated reading them in, and keys being committed are
//! validated at commit. So there are two ways to read a value: RawNames, which
//! takes nothing on faith and is for those two alone, and Dirent, which reads
//! a valid dirent with nothing to check.

#![allow(non_camel_case_types)]

use crate::btree::bkey::BkeySC;
use crate::btree::bkey_methods::{self, SetError};
use crate::btree::iter::TransBkey;
use crate::util::ffi::Opaque;
use crate::util::os_str::{OsStr, OsStrExt};
use crate::bkey_fsck_err_on;
use crate::c;
use crate::c::bch_validate_flags;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::typeinfo::AccessError;
use crate::util::Printbuf;
use core::ffi::{c_int, CStr};
use core::mem::offset_of;

use crate::cstructs::c as cs;
use crate::types::c_default;
use cstruct_macros::{bitfield, c_const, CStruct};
use nestify::nest;
use typeinfo_macros::TypeInfo;
use zerocopy::byteorder::little_endian as le;

// ── The target ───────────────────────────────────────────────────────────

/// What a dirent names, by d_type: for DT_SUBVOL, a subvolume (child) and
/// the subvolume the dirent lives in (parent); otherwise an inode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirentTarget {
    Inode(u64),
    Subvol { child: u32, parent: u32 },
}

impl c::bch_dirent {
    /// The inode number the dirent points at. Meaningless for a DT_SUBVOL
    /// dirent, which keeps subvolume IDs in the same space.
    pub fn d_inum(&self) -> u64 {
        (unsafe { self.target.d_inum }).get()
    }

    /// The target, read through the half of the union d_type says is live.
    pub fn target(&self) -> DirentTarget {
        if self.d_type() as u32 == c::DT_SUBVOL {
            let s = unsafe { self.target.subvol };
            DirentTarget::Subvol {
                child:  s.d_child_subvol.get(),
                parent: s.d_parent_subvol.get(),
            }
        } else {
            DirentTarget::Inode(self.d_inum())
        }
    }

    /// Set the subvolume a DT_SUBVOL dirent lives in.
    pub fn set_parent_subvol(&mut self, subvol: u32) {
        assert_eq!(self.d_type() as u32, c::DT_SUBVOL);
        self.target.subvol.d_parent_subvol = subvol.into();
    }
}

// ── The names ────────────────────────────────────────────────────────────

/// A dirent's names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirentName<'k> {
    Plain(&'k OsStr),
    /// In a casefolded directory: the name as given, and the casefolded name
    /// the dirent is hashed and looked up by.
    Casefolded { name: &'k OsStr, cf: &'k OsStr },
}

impl<'k> DirentName<'k> {
    /// The name as given: as bch2_dirent_get_name().
    pub fn name(&self) -> &'k OsStr {
        match *self {
            DirentName::Plain(name) | DirentName::Casefolded { name, .. } => name,
        }
    }

    /// The name the dirent is hashed and looked up by: as
    /// bch2_dirent_get_lookup_name().
    pub fn lookup_name(&self) -> &'k OsStr {
        match *self {
            DirentName::Plain(name)            => name,
            DirentName::Casefolded { cf, .. }  => cf,
        }
    }
}

/// Where the name block starts: d_name, or for a casefolded dirent
/// d_cf_name_block.d_names, past the lengths.
pub(super) const D_NAME_OFFSET:  usize = offset_of!(c::bch_dirent, name_block);
pub(super) const D_NAMES_OFFSET: usize = D_NAME_OFFSET +
    offset_of!(c::bch_dirent_cf_name_block, d_names);

/// A dirent's name block as an unvalidated value describes it, for validate
/// and to_text - see the top of the file. The lengths are as stored, and
/// name() and cf() check them against the value.
pub struct RawNames<'k> {
    /// The value from the start of the name block to its end.
    block:        &'k [u8],
    /// The block short of the NUL padding at the end of the value: as
    /// bch2_dirent_name_bytes().
    pub block_len: usize,
    pub casefold:  bool,
    pub name_len:  usize,
    /// 0 if not casefolded.
    pub cf_len:    usize,
}

impl<'k> RawNames<'k> {
    /// @k, a dirent at least min_val_size long, as bkey validate and to_text
    /// check before calling the type's.
    pub fn new(k: BkeySC<'k>) -> Self {
        let d = k.as_dirent().expect("a dirent");
        let v = k.val_bytes();
        let casefold = d.d_casefold() != 0;

        let start = if casefold { D_NAMES_OFFSET } else { D_NAME_OFFSET };
        let block = &v[start..];
        // Padding is to a u64, so it's only ever in the last one. On a
        // damaged key it can reach back past the start of the block: C's
        // unsigned arithmetic wraps there, and an empty block is what it means.
        let padding = v[v.len() - 8..].iter().rev().take_while(|&&b| b == 0).count();
        let block_len = block.len().saturating_sub(padding);

        let (name_len, cf_len) = if casefold {
            let cf = unsafe { &d.name_block.d_cf_name_block };
            (cf.d_name_len.get() as usize, cf.d_cf_name_len.get() as usize)
        } else {
            (block_len, 0)
        };

        RawNames { block, block_len, casefold, name_len, cf_len }
    }

    /// The name, if it's within the value.
    pub fn name(&self) -> Option<&'k [u8]> {
        self.block.get(..self.name_len)
    }

    /// The casefolded name, if the dirent is casefolded and it's within the
    /// value.
    pub fn cf(&self) -> Option<&'k [u8]> {
        if !self.casefold {
            return None;
        }
        self.block.get(self.name_len..self.name_len + self.cf_len)
    }

    /// Both names, if they're within the value.
    pub fn names(&self) -> Option<DirentName<'k>> {
        let name = OsStr::from_bytes(self.name()?);
        Some(if self.casefold {
            DirentName::Casefolded { name, cf: OsStr::from_bytes(self.cf()?) }
        } else {
            DirentName::Plain(name)
        })
    }
}

// ── A valid dirent ───────────────────────────────────────────────────────

/// A valid dirent - one that has passed validate, as every key from the
/// btree has.
#[derive(Clone, Copy)]
pub struct Dirent<'k> {
    k:     BkeySC<'k>,
    v:     &'k c::bch_dirent,
    names: DirentName<'k>,
}

impl<'k> Dirent<'k> {
    /// @k as a dirent, if it is one.
    pub fn new(k: BkeySC<'k>) -> Option<Self> {
        let v = k.as_dirent()?;
        let names = RawNames::new(k).names()
            .expect("dirent names within the value, as validate checked");
        Some(Dirent { k, v, names })
    }

    pub fn k(&self) -> BkeySC<'k> { self.k }

    pub fn v(&self) -> &'k c::bch_dirent { self.v }

    pub fn d_type(&self) -> u8 { self.v.d_type() }

    pub fn target(&self) -> DirentTarget { self.v.target() }

    pub fn names(&self) -> DirentName<'k> { self.names }

    /// The name as given.
    pub fn name(&self) -> &'k OsStr { self.names.name() }

    /// The name the dirent is hashed and looked up by.
    pub fn lookup_name(&self) -> &'k OsStr { self.names.lookup_name() }
}

// ── to_text ──────────────────────────────────────────────────────────────

/// @k, a dirent, as text: as bch2_dirent_to_text(). @k may not have passed
/// validate - a name that overruns the value is said to, not printed.
pub fn to_text(out: &mut Printbuf, k: BkeySC<'_>) {
    let d = k.as_dirent().expect("a dirent");
    let raw = RawNames::new(k);

    let Some(name) = raw.name() else {
        write!(out, "(invalid, dirent name overruns value)");
        return;
    };
    out.write_bytes(name);

    if raw.casefold {
        write!(out, " (casefold ");
        match raw.cf() {
            Some(cf) => out.write_bytes(cf),
            None     => write!(out, "(invalid, lookup name overruns value)"),
        }
        write!(out, ")");
    }

    match d.target() {
        DirentTarget::Inode(inum)              => write!(out, " -> {inum}"),
        DirentTarget::Subvol { child, parent } => write!(out, " -> {parent} -> {child}"),
    }

    write!(out, " type {}", d_type_str(d.d_type()));
}

/// @d_type's name: as bch2_d_type_str().
pub fn d_type_str(d_type: u8) -> &'static str {
    let name = if (d_type as u32) < c::BCH_DT_MAX {
        unsafe { *(&raw const c::bch2_d_types).cast::<*const core::ffi::c_char>().add(d_type as usize) }
    } else {
        core::ptr::null()
    };
    if name.is_null() {
        return "(bad d_type)";
    }
    unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("(invalid)")
}

/// For C's bkey_ops: bch2_dirent_to_text().
#[cold]
pub fn bch2_dirent_to_text(
    out: &mut Printbuf,
    _c:  &Opaque<c::bch_fs>,
    k:   BkeySC<'_>,
) {
    to_text(out, k)
}

// ── Validate: where a dirent becomes trusted ─────────────────────────────

/// Check @v.k, a dirent, as bch2_dirent_validate(): the dirent, now valid,
/// or the error the key fails with.
///
/// Only new keys are held to BCH_NAME_MAX: it used to be bigger, and older
/// keys can have longer names.
pub fn validate<'k>(v: &BkeyValidate<'_, 'k>) -> Result<Dirent<'k>, BchError> {
    let d = v.k.as_dirent().expect("a dirent");
    let raw = RawNames::new(v.k);

    bkey_fsck_err_on!(v, raw.name_len == 0, id::dirent_empty_name,
                      "empty name")?;

    bkey_fsck_err_on!(v, raw.name_len + raw.cf_len > raw.block_len, id::dirent_val_too_big,
                      "dirent names exceed bkey size ({} + {} > {})",
                      raw.name_len, raw.cf_len, raw.block_len)?;

    // Within the block, and so the value:
    let names = raw.names().expect("names within the name block");
    let name = names.name().as_bytes();

    bkey_fsck_err_on!(v, v.from.flags() & bch_validate_flags::BCH_VALIDATE_commit.bits() != 0 &&
                      name.len() > c::BCH_NAME_MAX as usize,
                      id::dirent_name_too_long,
                      "dirent name too big ({} > {})", name.len(), c::BCH_NAME_MAX)?;

    bkey_fsck_err_on!(v, name.contains(&0), id::dirent_name_embedded_nul,
                      "dirent has stray data after name's NUL")?;

    bkey_fsck_err_on!(v, name == b"." || name == b"..", id::dirent_name_dot_or_dotdot,
                      "invalid name")?;

    bkey_fsck_err_on!(v, name.contains(&b'/'), id::dirent_name_has_slash,
                      "name with /")?;

    bkey_fsck_err_on!(v, d.d_type() as u32 != c::DT_SUBVOL && d.d_inum() == v.k.k.p.inode,
                      id::dirent_to_itself,
                      "dirent points to own directory")?;

    if let DirentName::Casefolded { cf, .. } = names {
        let cf = cf.as_bytes();
        bkey_fsck_err_on!(v, v.from.from() == c::bkey_validate_from::BKEY_VALIDATE_commit as u32 &&
                          cf.len() > c::BCH_NAME_MAX as usize,
                          id::dirent_cf_name_too_big,
                          "dirent w/ cf name too big ({} > {})", cf.len(), c::BCH_NAME_MAX)?;

        bkey_fsck_err_on!(v, cf.contains(&0), id::dirent_stray_data_after_cf_name,
                          "dirent has stray data after cf name's NUL")?;
    }

    Ok(Dirent { k: v.k, v: d, names })
}

/// For C's bkey_ops: bch2_dirent_validate().
pub fn bch2_dirent_validate(
    c:    &Opaque<c::bch_fs>,
    k:    BkeySC<'_>,
    from: &c::bkey_validate_context,
) -> c_int {
    let fs = Fs::from_c(c);
    let v = BkeyValidate { fs: &fs, k, from };

    match validate(&v) {
        Ok(_)  => 0,
        Err(e) => -e.raw(),
    }
}

// ── Writing fields, for kvdb ─────────────────────────────────────────────

/// Set @field of dirent @k to @val, from text, for TransBkey::set(): see
/// btree/bkey_methods.rs.
///
/// The target is a union tagged by d_type - d_inum, or (d_child_subvol,
/// d_parent_subvol) for DT_SUBVOL - and d_type is a C bitfield: typeinfo
/// reaches neither. Each of these writes exactly the field named, tag or not,
/// so a test can make them disagree. d_name rewrites the name, not casefolded,
/// and leaves the key where it is - at the old name's offset, which is the
/// point: a key at the wrong offset, or a duplicate of another name.
pub fn set_field<'p>(k: &mut TransBkey<'_, '_>, fs: &Fs, field: &'p str, val: &'p str)
    -> Result<(), SetError<'p>>
{
    let overflow = |bytes, val| SetError::Access {
        field,
        err: AccessError::Overflow { bytes, val },
    };

    if field == "d_name" {
        return set_name(k, fs, val);
    }

    if !matches!(field, "d_type" | "d_inum" | "d_child_subvol" | "d_parent_subvol") {
        return k.set_fixed(field, val);
    }

    let v = bkey_methods::parse_val(k.k().type_, field, val)?;
    let d = k.k_i_mut().as_mut_dirent().expect("a dirent");

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
        "d_inum" => d.target.d_inum = v.into(),
        _ => {
            let v = u32::try_from(v).map_err(|_| overflow(4, v))?.to_le();
            let s = unsafe { &mut d.target.subvol };
            if field == "d_child_subvol" {
                s.d_child_subvol = v.into();
            } else {
                s.d_parent_subvol = v.into();
            }
        }
    }
    Ok(())
}

fn set_name<'p>(k: &mut TransBkey<'_, '_>, fs: &Fs, val: &'p str) -> Result<(), SetError<'p>> {
    if val.is_empty() || val.len() > c::BCH_NAME_MAX as usize {
        return Err(SetError::BadName { val });
    }

    // init_name() sizes the value down to the name, from whatever room the
    // key claims:
    let have_u64s = k.as_u64s().len();
    k.k_mut().u64s = have_u64s.min(u8::MAX as usize) as u8;

    // Only cf_encoding is read, and NULL means not casefolded:
    let hash_info = c::bch_hash_info::default();
    super::init_name(fs, k.k_i_mut(), &hash_info, OsStr::from_bytes(val.as_bytes()), None)
        .map_err(|_| SetError::BadName { val })
}
// ---- the data types of fs/dirent_format.h, which is generated from this file: see
// fs/types/lib.rs.

/*
 * Dirents (and xattrs) have to implement string lookups; since our b-tree
 * doesn't support arbitrary length strings for the key, we instead index by a
 * 64 bit hash (currently truncated sha1) of the string, stored in the offset
 * field of the key - using linear probing to resolve hash collisions. This also
 * provides us with the readdir cookie posix requires.
 *
 * Linear probing requires us to use whiteouts for deletions, in the event of a
 * collision:
 */
nest! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[repr(C, align(8))]
    #[c_packed]
    pub struct bch_dirent {
        pub v: cs::bch_val,

        /* Target inode number: */
        #[c_anon]
        #>[repr(C)]
        pub target: pub union bch_dirent_target {
            pub d_inum: le::U64,
            /* DT_SUBVOL */
            #[c_anon]
            #>[repr(C)]
            pub subvol: pub struct bch_dirent_subvol {
                pub d_child_subvol: le::U32,
                pub d_parent_subvol: le::U32,
            },
        },

        /*
         * Copy of mode bits 12-15 from the target inode - so userspace can get
         * the filetype without having to do a stat()
         */
        #[c_bitfield]
        #>[derive(Clone, Copy, CStruct, TypeInfo)]-
        #>[bitfield(u8)]
        pub d_type_bits: pub struct bch_dirent_d_type_bits {
            #[bits(5)]
            pub d_type: u8,
            #[bits(2)]
            pub d_unused: u8,
            #[bits(1)]
            pub d_casefold: u8,
        },

        #[c_anon]
        #>[repr(C, packed)]
        pub name_block: pub union bch_dirent_name_block {
            #[c_inline]
            #>[repr(C)]
            pub d_cf_name_block: pub struct bch_dirent_cf_name_block {
                pub d_pad: u8,
                /*
                 * C's padding, for __le16's alignment: zerocopy's le::U16 has
                 * none. The block's __packed is the member's, not its type's.
                 */
                #[c_anon("")]
                pub __d_name_len_align: u8,
                pub d_name_len: le::U16,
                pub d_cf_name_len: le::U16,
                pub d_names: [u8; 0],
            },
            pub d_name: [u8; 0],
        },
    }
}
c_default!(bch_dirent);
impl bch_dirent {
    pub fn d_type(&self) -> u8 { let b = self.d_type_bits; b.d_type() }
    pub fn set_d_type(&mut self, v: u8) { let mut b = self.d_type_bits; b.set_d_type(v); self.d_type_bits = b; }
    pub fn d_unused(&self) -> u8 { let b = self.d_type_bits; b.d_unused() }
    pub fn set_d_unused(&mut self, v: u8) { let mut b = self.d_type_bits; b.set_d_unused(v); self.d_type_bits = b; }
    pub fn d_casefold(&self) -> u8 { let b = self.d_type_bits; b.d_casefold() }
    pub fn set_d_casefold(&mut self, v: u8) { let mut b = self.d_type_bits; b.set_d_casefold(v); self.d_type_bits = b; }
}

c_const! {
    #[c_int]
    pub const DT_SUBVOL: u32 = 16;
}

c_const! {
    #[c_int]
    pub const BCH_DT_MAX: u32 = 17;
}

c_const! {
    #[c_int]
    pub const BCH_NAME_MAX: u32 = 512;
}
