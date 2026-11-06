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
use crate::btree::iter::{BtreeIter, BtreeTrans, NoRoom, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::init::error::BkeyValidate;
use crate::c;
use crate::util::Printbuf;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::{dirent, inode, prt};
use crate::util::ffi::Opaque;
use crate::snapshot_states::{SNAPSHOT_STATE_VALUES, SUBVOLUME_STATE_VALUES};
use crate::typeinfo::{self, AccessError, FieldTarget, ResolveError, TypeInfo};
use core::fmt;
use core::mem::{size_of, MaybeUninit};
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_export;
use crate::format::{Bpos, Btree};

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
    pub fn set_error(&mut self, fs: &Fs, err: c::bch_key_type_errors) {
        assert!(self.as_u64s().len() * 8 >= size_of::<c::bkey>() + size_of::<c::Error>(),
                "no room in the key's buffer for an error key");
        self.k_mut().type_ = c::bch_bkey_type::KEY_TYPE_error.0 as u8;

        if fs.request_incompat_feature(
            c::bcachefs_metadata_version::bcachefs_metadata_version_extented_key_type_error).is_ok() {
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
        } else if self.k().type_ as u32 == c::bch_bkey_type::KEY_TYPE_dirent.0 {
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
        let info = crate::btree::bkey::key_type(type_).map(|t| t.info)
            .ok_or(SetError::Unsupported { type_ })?;
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

// ── Triggers ─────────────────────────────────────────────────────────────

/// What a key type's trigger is called with. @new is mutable: an atomic
/// trigger can edit the key going in, in place.
///
/// XXX: TriggerOp ought to be generic over its key type V, @new a V::Mut -
/// grown to MutBkey::MIN_BYTES before any trigger runs, as
/// bkey_make_mut_noupdate()'s copy is, the fields an older value lacks
/// zeroed - so a trigger only ever edits fields in place. Today one can grow the key:
/// bch2_trigger_get_mutable_new() moves the queued update to a bigger buffer
/// mid-commit, and refuses in the atomic phase. That isn't necessarily safe,
/// and a trigger shouldn't have to.
pub struct TriggerOp<'a> {
    pub btree: Btree,
    pub level: u32,
    pub old:   BkeySC<'a>,
    pub new:   BkeyS<'a>,
    pub flags: UpdateTriggerFlags,
    /// The room @new has - for a C trigger it's handed back to
    new_buf_u64s: u32,
}

impl<'a> TriggerOp<'a> {
    /// # Safety
    /// @op is the one the trigger was called with: both keys valid, and @new
    /// the trigger's to edit, for 'a.
    pub unsafe fn from_raw(op: &'a mut c::btree_trigger_op) -> Self {
        let new = unsafe { &mut op.new.split.kv };
        TriggerOp {
            btree: op.btree,
            level: op.level,
            old:   unsafe { BkeySC::from_c(&op.old) },
            new:   BkeyS { k: unsafe { &mut *new.k }, v: new.v, val: core::marker::PhantomData },
            flags: UpdateTriggerFlags::from_bits_retain(op.flags.bits()),
            new_buf_u64s: op.new_buf_u64s,
        }
    }

    /// As C's btree_trigger_op, for a C trigger this one runs: @new lent to it
    /// for as long as this is borrowed.
    pub(crate) fn to_raw(&mut self) -> c::btree_trigger_op {
        c::btree_trigger_op {
            btree:        self.btree,
            level:        self.level,
            old:          self.old.into(),
            new:          self.new.as_raw(),
            new_buf_u64s: self.new_buf_u64s,
            flags:        c::btree_iter_update_trigger_flags::from_bits_retain(self.flags.bits()),
        }
    }

    /// @new with room for @needed_u64s, to edit: the queued update moved to a
    /// bigger buffer if it hasn't the room - bch2_trigger_get_mutable_new() -
    /// and @new repointed there. An insert's, outside the atomic phase.
    ///
    /// The key borrows the op: it can be @new itself.
    pub fn new_with_room<'s, 't>(&'s mut self, t: &TransAttempt<'s, 't>, needed_u64s: u32)
        -> Result<TransBkey<'s, 't>, BchError>
    {
        let mut out = MaybeUninit::<c::bkey_s>::uninit();
        // SAFETY: the op the trigger was called with, in its commit
        t.result(unsafe { c::bch2_trigger_get_mutable_new(t.raw(), self.to_raw(), needed_u64s, out.as_mut_ptr()) })?;
        // SAFETY: written on success - the update's key, in transaction
        // memory, which outlives the op
        let out = unsafe { out.assume_init().split.kv };
        self.new = BkeyS { k: unsafe { &mut *out.k }, v: out.v, val: core::marker::PhantomData };
        self.new_buf_u64s = self.new_buf_u64s.max(needed_u64s);

        // SAFETY: @new, @new_buf_u64s long, the trigger's to edit while the
        // attempt runs
        Ok(unsafe { TransBkey::from_raw_room(t, core::ptr::from_mut(self.new.k).cast(), self.new_buf_u64s) })
    }

    /// Run @f on the key going out, as an overwrite, then on the key coming
    /// in, as an insert - each that isn't a deletion: as C's
    /// trigger_run_overwrite_then_insert().
    pub fn run_overwrite_then_insert<F>(&self, mut f: F) -> Result<(), BchError>
    where
        F: FnMut(BkeySC<'_>, UpdateTriggerFlags) -> Result<(), BchError>,
    {
        if !self.old.k.key_type().is_deleted() {
            f(self.old, self.flags - UpdateTriggerFlags::INSERT)?;
        }
        if !self.new.k.key_type().is_deleted() {
            f(BkeySC::from(&self.new).untyped(), self.flags - UpdateTriggerFlags::OVERWRITE)?;
        }
        Ok(())
    }
}

/// A key type's methods, on its value's type: each a no-op where the type
/// has none.
pub trait BkeyOps {
    fn validate(_v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        Ok(())
    }

    /// The value, which may not have validated: printing one that didn't is
    /// the point - only its minimum size is checked first.
    fn to_text(_out: &mut Printbuf, _fs: Option<&Fs>, _k: BkeySC<'_>) {}

    /// Its fields byte swapped, read from a filesystem of the other endianness.
    fn swab(_fs: Option<&Fs>, _k: BkeyS<'_>) {}

    /// @r merged into @l, which it follows - false if it can't be.
    fn merge(_fs: &Fs, _l: BkeyS<'_>, _r: BkeySC<'_>) -> bool {
        false
    }

    /// An older version's value made the current one, on read, or the other
    /// way on write.
    fn compat(_btree: Btree, _version: c::bcachefs_metadata_version, _big_endian: bool, _write: bool,
              _k: BkeyS<'_>) {}

    fn check_repair(_t: &TransAttempt<'_, '_>, _iter: &BtreeIter<'_>, _btree: Btree, _level: u32,
                    _k: BkeySC<'_>) -> Result<(), BchError> {
        Ok(())
    }

    const TRIGGER: Option<&'static Trigger> = None;
}

/// A key type's trigger - a static: types share a trigger, and an update from
/// one to another of them runs it once, with both keys, so it's compared by
/// address, which a static's is unique to it and a fn's isn't.
pub struct Trigger(pub fn(&TransAttempt<'_, '_>, &mut TriggerOp<'_>) -> Result<(), BchError>);

/// A key type's BkeyOps, as its KeyType row has them.
#[derive(Clone, Copy)]
pub struct Ops {
    pub validate:     fn(&BkeyValidate<'_, '_>) -> Result<(), BchError>,
    pub to_text:      fn(&mut Printbuf, Option<&Fs>, BkeySC<'_>),
    pub swab:         fn(Option<&Fs>, BkeyS<'_>),
    pub merge:        fn(&Fs, BkeyS<'_>, BkeySC<'_>) -> bool,
    pub compat:       fn(Btree, c::bcachefs_metadata_version, bool, bool, BkeyS<'_>),
    pub check_repair: fn(&TransAttempt<'_, '_>, &BtreeIter<'_>, Btree, u32, BkeySC<'_>) -> Result<(), BchError>,
    pub trigger:      Option<&'static Trigger>,
}

/// A value that's nothing but its key: hash_whiteout's, set's.
fn empty_val_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let n = v.k.k.val_bytes();
    crate::bkey_fsck_err_on!(v, n != 0, crate::init::error::id::bkey_val_size_nonzero,
                             "incorrect value size ({n} != 0)")
}

impl BkeyOps for c::Deleted {}
impl BkeyOps for c::Whiteout {}
impl BkeyOps for c::ExtentWhiteout {}

impl BkeyOps for c::Error {
    /// Why it's an error: none in a value from before there was a reason.
    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        let err = k.val_bytes().get(core::mem::offset_of!(c::Error, repr.err)).copied().unwrap_or(0);
        crate::util::ToText::to_text(&c::bch_key_type_errors(err.into()), out)
    }
}

impl BkeyOps for c::Cookie {
    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        crate::prt!(out, "{}", k.arm::<c::Cookie>().expect("a cookie").v().cookie.get());
    }
}

impl BkeyOps for c::HashWhiteout {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        empty_val_validate(v)
    }
}

impl BkeyOps for c::Set {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        empty_val_validate(v)
    }

    fn merge(_fs: &Fs, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
        l.k.resize(l.k.size + r.k.size);
        true
    }
}

/// bch2_bpos_to_text().
#[cold]
pub fn bpos_to_text(out: &mut Printbuf, p: Bpos) {
    use crate::btree::bkey::{POS_MAX, POS_MIN, SPOS_MAX};

    if p == POS_MIN {
        prt!(out, "POS_MIN");
    } else if p == POS_MAX {
        prt!(out, "POS_MAX");
    } else if p == SPOS_MAX {
        prt!(out, "SPOS_MAX");
    } else {
        match p.inode {
            u64::MAX => prt!(out, "U64_MAX:"),
            i        => prt!(out, "{i}:"),
        }
        match p.offset {
            u64::MAX => prt!(out, "U64_MAX:"),
            o        => prt!(out, "{o}:"),
        }
        match p.snapshot {
            u32::MAX => prt!(out, "U32_MAX"),
            s        => prt!(out, "{s}"),
        }
    }
}

#[c_export]
pub fn bch2_bpos_to_text(out: &mut Printbuf, pos: Bpos) {
    bpos_to_text(out, pos)
}

/// bch2_bkey_to_text(): the header.
#[cold]
pub fn bkey_to_text(out: &mut Printbuf, k: &c::bkey) {
    prt!(out, "u64s {} type ", k.u64s);
    match crate::btree::bkey::key_type(k.type_) {
        Some(t) => prt!(out, "{} ", t.name),
        None    => prt!(out, "{} ", k.type_),
    }
    bpos_to_text(out, k.p);
    prt!(out, " len {} ver {}", k.size, { k.bversion.lo });
}

#[c_export]
pub fn bch2_bkey_to_text(out: &mut Printbuf, k: Option<&c::bkey>) {
    match k {
        Some(k) => bkey_to_text(out, k),
        None    => prt!(out, "(null)"),
    }
}

/// bch2_val_to_text(): the value, by its type's to_text - which may not have
/// validated: printing one that didn't is the point.
#[cold]
pub fn val_to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
    /*
     * Printing a key that failed to validate is the entire point of being
     * able to print it, so val_to_text() methods run on unvalidated values
     * and can't be given a length they may not trust.
     *
     * bkey_val_u64s() is (u64s - BKEY_U64s): a __u8 against a size_t, so a
     * truncated header doesn't yield a small length, it yields a huge one.
     * bch2_bkey_val_to_text()'s "does it have a value?" test is then
     * satisfied by precisely the corruption it ought to be catching.
     */
    if usize::from(k.k.u64s) < crate::btree::bkey::BKEY_U64S {
        prt!(out, "(truncated key header: {} u64s)", k.k.u64s);
        return;
    }

    let min = min_val_size(k.k.type_);
    if k.k.val_bytes() < min {
        prt!(out, "(value too small: {} < {min})", k.k.val_bytes());
        return;
    }

    (key_ops(k.k.type_).to_text)(out, fs, k)
}

#[c_export]
pub fn bch2_val_to_text(out: &mut Printbuf, c: Option<&Opaque<Fs>>, k: BkeySC<'_>) {
    val_to_text(out, c.map(Fs::from_c), k)
}

/// bch2_bkey_val_to_text(): the header, and the value.
#[cold]
pub fn bkey_val_to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
    if usize::from(k.k.u64s) < crate::btree::bkey::BKEY_U64S {
        prt!(out, "(invalid, u64s {} < {})", k.k.u64s, crate::btree::bkey::BKEY_U64S);
        return;
    }

    bkey_to_text(out, k.k);

    if k.k.val_bytes() != 0 {
        let mut out = out.indent(2);
        prt!(out, ": ");
        val_to_text(&mut out, fs, k);
    }
}

#[c_export]
pub fn bch2_bkey_val_to_text(out: &mut Printbuf, c: Option<&Opaque<Fs>>, k: BkeySC<'_>) {
    bkey_val_to_text(out, c.map(Fs::from_c), k)
}

/// The shortest valid value of type @ty: its struct up to BCH_BKEY_TYPES()'
/// field, or all of it. A type this code doesn't know has no minimum.
pub fn min_val_size(ty: u8) -> usize {
    crate::btree::bkey::key_type(ty).map_or(0, |t| t.min_val_size)
}

/// bch2_bkey_val_validate(): the value, by its type's validate - checked
/// first that it's as long as the type's shortest.
pub fn bkey_val_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    if v.fs.flags.get(c::bch_fs_flags::BCH_FS_no_invalid_checks) {
        return Ok(());
    }

    let (have, min) = (v.k.k.val_bytes(), min_val_size(v.k.k.type_));
    crate::bkey_fsck_err_on!(v, have < min, crate::init::error::id::bkey_val_size_too_small,
                             "bad val size ({have} < {min})")?;

    (key_ops(v.k.k.type_).validate)(v)
}

#[c_export]
pub fn bch2_bkey_val_validate(c: &Opaque<Fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(bkey_val_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

/// bch2_btree_node_type_str(): an interior node's, or a leaf's btree's.
struct NodeType(Option<Btree>);

impl fmt::Display for NodeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None    => f.write_str("internal btree node"),
            Some(b) => write!(f, "{b}"),
        }
    }
}

/// __bch2_bkey_validate(): the key's header - its type allowed in its btree,
/// its size and position what its btree's keys' are.
pub fn bkey_header_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    use crate::init::error::id;
    use crate::bkey_fsck_err_on;
    let k = v.k.k;

    if v.fs.flags.get(c::bch_fs_flags::BCH_FS_no_invalid_checks) {
        return Ok(());
    }

    bkey_fsck_err_on!(v, usize::from(k.u64s) < crate::btree::bkey::BKEY_U64S, id::bkey_u64s_too_small,
                      "u64s too small ({} < {})", k.u64s, crate::btree::bkey::BKEY_U64S)?;

    // a leaf's btree, None for an interior node; past the btrees this code
    // knows, nothing's known of its keys
    let btree = (v.from.level == 0).then_some(v.from.btree);
    if btree.is_some_and(|b| b.0 >= c::BTREE_ID_NR as u32) {
        return Ok(());
    }

    let kt = crate::btree::bkey::key_type(k.type_);
    let strict = v.from.bits.flags().contains(c::bch_validate_flags::BCH_VALIDATE_commit) ||
        btree.is_none() ||
        kt.is_some_and(|t| t.flags & c::BKEY_TYPE_strict_btree_checks != 0);
    let allowed = match btree {
        None    => [c::bch_bkey_type::KEY_TYPE_deleted, c::bch_bkey_type::KEY_TYPE_btree_ptr,
                    c::bch_bkey_type::KEY_TYPE_btree_ptr_v2].iter().fold(0, |m, t| m | 1 << t.0),
        Some(b) => b.key_types(),
    };

    bkey_fsck_err_on!(v, strict && kt.is_some() && allowed & (1 << k.type_) == 0,
                      id::bkey_invalid_type_for_btree, "invalid key type for btree {} ({})",
                      NodeType(btree), kt.map_or("(unknown)", |t| t.name))?;

    if btree.is_some_and(|b| b.is_extents()) && !k.key_type().is_extent_whiteout() {
        bkey_fsck_err_on!(v, k.size == 0, id::bkey_extent_size_zero, "size == 0")?;
        bkey_fsck_err_on!(v, u64::from(k.size) > k.p.offset, id::bkey_extent_size_greater_than_offset,
                          "size greater than offset ({} > {})", k.size, { k.p.offset })?;
    } else {
        bkey_fsck_err_on!(v, k.size != 0, id::bkey_size_nonzero, "size != 0")?;
    }

    if let Some(b) = btree {
        if b.has_snapshots() {
            bkey_fsck_err_on!(v, k.p.snapshot == 0, id::bkey_snapshot_zero, "snapshot == 0")?;
        } else if !b.has_snapshot_field() {
            bkey_fsck_err_on!(v, k.p.snapshot != 0, id::bkey_snapshot_nonzero, "nonzero snapshot")?;
        } else {
            /*
             * btree uses snapshot field but it's not required to be
             * nonzero
             */
        }

        bkey_fsck_err_on!(v, k.p == crate::btree::bkey::POS_MAX, id::bkey_at_pos_max, "key at POS_MAX")?;
    }
    Ok(())
}

#[c_export]
pub fn __bch2_bkey_validate(c: &Opaque<Fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(bkey_header_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

/// bch2_bkey_validate(): the header, then the value.
pub fn bkey_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    bkey_header_validate(v)?;
    bkey_val_validate(v)
}

#[c_export]
pub fn bch2_bkey_validate(c: &Opaque<Fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(bkey_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

/// bch2_bkey_swab_val(): the value byte swapped, by its type's swab.
pub fn bkey_swab_val(fs: Option<&Fs>, k: BkeyS<'_>) {
    (key_ops(k.k.type_).swab)(fs, k)
}

#[c_export]
pub fn bch2_bkey_swab_val(c: Option<&Fs>, k: BkeyS<'_>) {
    bkey_swab_val(c, k)
}

/// bch2_bkey_maybe_mergable(): @r might be merged into @l - the same type and
/// version, @r starting where @l ends.
fn maybe_mergable(l: &c::bkey, r: &c::bkey) -> bool {
    l.type_ == r.type_ && { l.bversion } == { r.bversion } && l.p == r.start_pos()
}

/// bch2_bkey_merge(): @r merged into @l, by its type's merge - if they're
/// mergable, and the result isn't too big for a key.
pub fn bkey_merge(fs: &Fs, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
    maybe_mergable(l.k, r.k) &&
        u64::from(l.k.size) + u64::from(r.k.size) <= u64::from(u32::MAX) &&
        !crate::debug::params::key_merging_disabled() &&
        (key_ops(l.k.type_).merge)(fs, l, r)
}

#[c_export]
pub fn bch2_bkey_merge(c: &Opaque<Fs>, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
    bkey_merge(Fs::from_c(c), l, r)
}

/// A key type's methods, by its number: none for a type this code doesn't know.
pub fn key_ops(ty: u8) -> &'static Ops {
    enum Unknown {}
    impl BkeyOps for Unknown {}
    static UNKNOWN: Ops = ops::<Unknown>();

    crate::btree::bkey::key_type(ty).map_or(&UNKNOWN, |t| &t.ops)
}

/// The same trigger - or none, both.
fn same_trigger(a: Option<&'static Trigger>, b: Option<&'static Trigger>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => core::ptr::eq(a, b),
        (a, b)             => a.is_none() && b.is_none(),
    }
}

/// # Safety
/// @op is one the commit is running triggers for: both keys valid, @new the
/// trigger's to edit.
unsafe fn run_trigger(t: &TransAttempt<'_, '_>, trigger: Option<&'static Trigger>, op: &mut c::btree_trigger_op)
    -> Result<(), BchError>
{
    match trigger {
        Some(trigger) => (trigger.0)(t, &mut unsafe { TriggerOp::from_raw(op) }),
        None          => Ok(()),
    }
}

/// bch2_key_trigger(): @op's key's trigger - the old key's type's, the new
/// key's if it's a deletion - then its snapshot's count of keys.
///
/// # Safety
/// As run_trigger().
unsafe fn key_trigger(t: &TransAttempt<'_, '_>, op: &mut c::btree_trigger_op) -> Result<(), BchError> {
    let ty = match unsafe { (*op.old.k).type_ } {
        0  => unsafe { (*op.new.split.kv.k).type_ },
        ty => ty,
    };
    unsafe { run_trigger(t, key_ops(ty).trigger, op) }?;

    /*
     * Gate matches the trans-commit dispatch: only snapshotted btrees get
     * per-snapshot key counts. Snapshot-field btrees (reconcile_pending)
     * carry a snapshot in their bpos but must not be counted.
     */
    if op.btree.has_snapshots() {
        crate::alloc::buckets::trigger_snapshot_nr_keys(t, &unsafe { TriggerOp::from_raw(op) })?;
    }
    Ok(())
}

/// # Safety
/// @op is one C runs a trigger for, in its commit's attempt.
#[c_export]
pub unsafe fn bch2_key_trigger(trans: &BtreeTrans<'_>, mut op: c::btree_trigger_op) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(unsafe { key_trigger(&t, &mut op) })
}

#[c_export]
pub fn bch2_bkey_check_repair<'t>(trans: &'t BtreeTrans<'t>, iter: &mut BtreeIter<'t>, btree: Btree,
                                  level: c_uint, k: BkeySC<'_>) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c((key_ops(k.k.type_).check_repair)(&t, iter, btree, level, k))
}

/// A deletion at @p, for the side of an update that has no key.
fn deleted_at(p: Bpos) -> c::bkey_i {
    c::bkey_i { k: c::bkey { p, ..c::bkey::new() }, ..Default::default() }
}

fn raw_s(k: &mut c::bkey_i) -> c::bkey_s {
    c::bkey_s { split: c::bkey_s_split { kv: c::bkey_s_kv { k: &mut k.k, v: &mut k.v } } }
}

fn raw_s_c(k: &c::bkey_i) -> c::bkey_s_c {
    c::bkey_s_c { k: &k.k, v: &k.v }
}

fn raw_flags(f: UpdateTriggerFlags) -> c::btree_iter_update_trigger_flags {
    c::btree_iter_update_trigger_flags::from_bits_retain(f.bits())
}

/// run_one_mem_trigger(): @i's triggers in the atomic phase, or gc's, with
/// @flags - its snapshot's count of keys first, from its old key to its new,
/// then its key type's trigger, once, with both keys, where the old and new
/// types share one.
///
/// # Safety
/// @i is an update of @trans's, in its commit.
#[c_export]
pub unsafe fn bch2_run_one_mem_trigger(trans: &BtreeTrans<'_>, i: &mut c::btree_insert_entry, flags: c_uint)
    -> c_int
{
    let t = unsafe { trans.attempt_in_progress() };
    let flags = UpdateTriggerFlags::from_bits_retain(flags);
    let new = unsafe { &mut *i.k };
    let mut deleted = deleted_at(new.k.p);
    let old_k = i.old_k;
    let (old_ops, new_ops) = (key_ops(old_k.type_), key_ops(new.k.type_));

    let mut op = c::btree_trigger_op {
        btree:        i.bits.btree_id(),
        level:        i.bits.level().into(),
        old:          c::bkey_s_c { k: &old_k, v: i.old_v },
        new:          raw_s(new),
        new_buf_u64s: i.k_buf_u64s.into(),
        flags:        raw_flags(flags),
    };

    let ret = (|| {
        // a net count, old to new: before the split below rewrites @op
        if op.btree.has_snapshots() {
            crate::alloc::buckets::trigger_snapshot_nr_keys(&t, &unsafe { TriggerOp::from_raw(&mut op) })?;
        }

        if old_ops.trigger.is_some() && same_trigger(old_ops.trigger, new_ops.trigger) {
            op.flags = raw_flags(flags | UpdateTriggerFlags::INSERT | UpdateTriggerFlags::OVERWRITE);
            return unsafe { run_trigger(&t, old_ops.trigger, &mut op) };
        }

        if new_ops.trigger.is_some() {
            op.flags = raw_flags(flags | UpdateTriggerFlags::INSERT);
            op.old   = raw_s_c(&deleted);
            unsafe { run_trigger(&t, new_ops.trigger, &mut op) }?;
        }

        if old_ops.trigger.is_some() {
            op.flags        = raw_flags(flags | UpdateTriggerFlags::OVERWRITE);
            op.old          = c::bkey_s_c { k: &old_k, v: i.old_v };
            op.new          = raw_s(&mut deleted);
            op.new_buf_u64s = 0;
            unsafe { run_trigger(&t, old_ops.trigger, &mut op) }?;
        }
        Ok(())
    })();
    ret_to_c(ret)
}

/// run_one_trans_trigger(): the next of @i's transactional triggers not run -
/// both at once where the old and new types share one, else the overwrite,
/// then the insert - then its snapshot's count of keys. A trigger can queue
/// more updates, so it's handed copies, not @i.
///
/// # Safety
/// As bch2_run_one_mem_trigger().
#[c_export]
pub unsafe fn bch2_run_one_trans_trigger(trans: &BtreeTrans<'_>, i: &mut c::btree_insert_entry) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    let new = unsafe { &mut *i.k };
    let mut deleted = deleted_at(new.k.p);
    let old_k = i.old_k;
    let (old_ops, new_ops) = (key_ops(old_k.type_), key_ops(new.k.type_));
    let mut flags = UpdateTriggerFlags::from_bits_retain(i.flags) | UpdateTriggerFlags::TRANSACTIONAL;

    let mut op = c::btree_trigger_op {
        btree:        i.bits.btree_id(),
        level:        i.bits.level().into(),
        old:          c::bkey_s_c { k: &old_k, v: i.old_v },
        new:          raw_s(new),
        new_buf_u64s: i.k_buf_u64s.into(),
        flags:        raw_flags(flags),
    };

    let mut trigger = old_ops.trigger;
    if !i.bits.insert_trigger_run() && !i.bits.overwrite_trigger_run() &&
       same_trigger(old_ops.trigger, new_ops.trigger) {
        i.bits.set_overwrite_trigger_run(true);
        i.bits.set_insert_trigger_run(true);
        flags |= UpdateTriggerFlags::INSERT | UpdateTriggerFlags::OVERWRITE;
    } else if !i.bits.overwrite_trigger_run() {
        i.bits.set_overwrite_trigger_run(true);
        flags |= UpdateTriggerFlags::OVERWRITE;
        op.new = raw_s(&mut deleted);
        op.new_buf_u64s = 0;
    } else if !i.bits.insert_trigger_run() {
        i.bits.set_insert_trigger_run(true);
        flags |= UpdateTriggerFlags::INSERT;
        op.old = raw_s_c(&deleted);
        trigger = new_ops.trigger;
    }
    op.flags = raw_flags(flags);

    let ret = unsafe { run_trigger(&t, trigger, &mut op) }.and_then(|()| {
        if op.btree.has_snapshots() {
            crate::alloc::buckets::trigger_snapshot_nr_keys(&t, &unsafe { TriggerOp::from_raw(&mut op) })
        } else {
            Ok(())
        }
    });
    ret_to_c(ret)
}

pub const fn ops<T: BkeyOps>() -> Ops {
    Ops {
        validate:     T::validate,
        to_text:      T::to_text,
        swab:         T::swab,
        merge:        T::merge,
        compat:       T::compat,
        check_repair: T::check_repair,
        trigger:      T::TRIGGER,
    }
}
