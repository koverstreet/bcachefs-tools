// SPDX-License-Identifier: GPL-2.0

use crate::format::BkeyType;
use crate::btree::bkey::{BkeyS, BkeySC};
use crate::btree::bkey::prelude::*;
use crate::btree::iter::{BtreeIter, BtreeTrans, TransAttempt};
use crate::btree::trigger::Trigger;
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::init::error::BkeyValidate;
use crate::prt;
use crate::util::Printbuf;
use crate::util::ffi::Opaque;
use core::fmt;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_export;

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

/// The shortest valid value of type @ty: its struct up to its #[grown(from)]
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
        kt.is_some_and(|t| t.flags & c::BKEY_TYPE_generic == 0);
    let allowed = match btree {
        None    => [BkeyType::deleted, BkeyType::btree_ptr,
                    BkeyType::btree_ptr_v2].iter().fold(0, |m, t| m | 1 << t.0),
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

/// @r might be merged into @l - the same type and version, @r starting where
/// @l ends.
#[c_export]
pub fn bch2_bkey_maybe_mergable(l: &c::bkey, r: &c::bkey) -> bool {
    l.type_ == r.type_ && { l.bversion } == { r.bversion } && l.p == r.start_pos()
}

/// bch2_bkey_merge(): @r merged into @l, by its type's merge - if they're
/// mergable, and the result isn't too big for a key.
pub fn bkey_merge(fs: &Fs, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
    bch2_bkey_maybe_mergable(l.k, r.k) &&
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

#[c_export]
pub fn bch2_bkey_check_repair<'t>(trans: &'t BtreeTrans<'t>, iter: &mut BtreeIter<'t>, btree: Btree,
                                  level: c_uint, k: BkeySC<'_>) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c((key_ops(k.k.type_).check_repair)(&t, iter, btree, level, k))
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
