// SPDX-License-Identifier: GPL-2.0

//! Alloc keys - background.c's, as it converts.
//!
//! An alloc key has had four versions. v1 packs its fields at fixed widths,
//! a bit in @fields for each present; v2 and v3 have them as varints, and
//! each must fit the width BCH_ALLOC_FIELDS_V2() gives it; v4 is a plain
//! struct, grown since - a value shorter than today's struct reads as zeroes
//! past its end. Every user reads one as a v4: alloc_to_v4(); an edit makes
//! it one - alloc_to_v4_mut() - and an update, start_alloc_update(), edits
//! the update a transaction has already queued for the bucket, so triggers
//! on one bucket each edit the same key.
//!
//! bucket_gens keys hold a generation for each of KEY_TYPE_BUCKET_GENS_NR
//! buckets; alloc_read loads a device's generations from them - or, before
//! they existed, from the alloc keys.
//!
//! Where a bucket belongs in the lrus, and in need_discard, freespace and
//! bucket_gens, are background.h's inlines, for Rust, while C still has its
//! own: the alloc trigger writes the entries by C's, fsck checks them by
//! these. The two must agree exactly - to the C integer types, wrapping and
//! all - or fsck calls good entries bad.

use crate::accounting::{bucket_data_type, data_type, data_type_from_u8, data_type_is_empty, data_type_movable};
use crate::alloc::buckets::{bucket_to_u64, u64_to_bucket};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeyS, BkeySC, BkeyWhole, POS_MIN};
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, LoopFlow, TransAttempt,
                         TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{err_ptr, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::opts::{data_type_str, prt_data_type};
use crate::util::ffi::Opaque;
use crate::util::kernel::{READ, WRITE};
use crate::util::rcu::read_lock;
use crate::util::varint;
use crate::util::vstructs::Validated;
use crate::util::Printbuf;
use crate::{bkey_fsck_err_on, btree_trans, prt};
use core::ffi::c_int;
use core::mem::{offset_of, size_of};
use core::sync::atomic::Ordering;
use cstruct_macros::c_export;

// ── Alloc keys before v4 ─────────────────────────────────────────────────

macro_rules! uint {
    (8) => { u8 };
    (16) => { u16 };
    (32) => { u32 };
    (64) => { u64 };
}

macro_rules! alloc_unpacked {
    ($(($name:ident, $bits:tt)),* $(,)?) => {
        /// An alloc key from before v4, unpacked: bkey_alloc_unpacked. Its
        /// varint fields are BCH_ALLOC_FIELDS_V2()'s, each as wide.
        #[derive(Default)]
        struct AllocUnpacked {
            journal_seq:  u64,
            generation:   u8,
            oldest_gen:   u8,
            data_type:    u8,
            need_discard: bool,
            $($name: uint!($bits),)*
        }

        impl AllocUnpacked {
            /// v2's and v3's fields, @nr of them varints at @data, the rest
            /// 0: None if one doesn't decode, or doesn't fit its field.
            fn varint_fields(mut self, mut data: &[u8], nr: u8) -> Option<Self> {
                let mut v = [0u64; [$(stringify!($name)),*].len()];
                for f in v.iter_mut().take(nr.into()) {
                    let (x, n) = varint::decode(data).ok()?;
                    *f = x;
                    data = &data[n..];
                }

                let [$($name),*] = v;
                $(self.$name = $name.try_into().ok()?;)*
                Some(self)
            }
        }
    };
}
c::BCH_ALLOC_FIELDS_V2!(alloc_unpacked);

macro_rules! alloc_v1_fields {
    ($(($name:ident, $bits:tt)),* $(,)?) => {
        /// The width of each of v1's fields, in bytes.
        const ALLOC_V1_FIELD_BYTES: &[usize] = &[$($bits / 8),*];

        impl AllocUnpacked {
            /// v1's fields, those present - @fields' bits - little endian at
            /// @data, each its width: alloc_field_v1_get(). Those past a value
            /// cut short are 0: validate refuses it, and to_text prints it.
            fn v1_fields(mut self, fields: u8, mut data: &[u8]) -> Self {
                let mut v = [0u64; ALLOC_V1_FIELD_BYTES.len()];
                for (i, (f, &bytes)) in v.iter_mut().zip(ALLOC_V1_FIELD_BYTES).enumerate() {
                    if fields & 1 << i != 0 {
                        let Some(b) = data.get(..bytes) else { break };
                        let mut le = [0u8; 8];
                        le[..bytes].copy_from_slice(b);
                        *f = u64::from_le_bytes(le);
                        data = &data[bytes..];
                    }
                }

                // no wider than v2's, which they're unpacked as
                let [$($name),*] = v;
                $(self.$name = $name as _;)*
                self
            }
        }
    };
}
c::BCH_ALLOC_FIELDS_V1!(alloc_v1_fields);

impl AllocUnpacked {
    fn v1(a: BkeySC<'_, c::Alloc<Validated>>) -> Self {
        let v = a.v();
        Self { generation: v.generation, ..Default::default() }.v1_fields(v.fields, &v.data)
    }

    fn v2(a: BkeySC<'_, c::AllocV2<Validated>>) -> Option<Self> {
        let v = a.v();
        Self {
            generation: v.generation,
            oldest_gen: v.oldest_gen,
            data_type:  v.data_type,
            ..Default::default()
        }.varint_fields(&v.data, v.nr_fields)
    }

    fn v3(a: BkeySC<'_, c::AllocV3<Validated>>) -> Option<Self> {
        let v = a.v();
        Self {
            journal_seq:  v.journal_seq.get(),
            generation:   v.generation,
            oldest_gen:   v.oldest_gen,
            data_type:    v.data_type,
            need_discard: v.need_discard(),
            ..Default::default()
        }.varint_fields(&v.data, v.nr_fields)
    }

    /// @k, an alloc key before v4 - zeroed, if it isn't one: bch2_alloc_unpack().
    /// Validate has seen v2 and v3 unpack.
    fn new(k: BkeySC<'_>) -> Self {
        if let Some(a) = k.arm::<c::Alloc>() {
            Self::v1(a)
        } else if let Some(a) = k.arm::<c::AllocV2>() {
            Self::v2(a).unwrap_or_default()
        } else if let Some(a) = k.arm::<c::AllocV3>() {
            Self::v3(a).unwrap_or_default()
        } else {
            Self::default()
        }
    }
}

/// v1's value size, in u64s, by the fields it says it has:
/// bch_alloc_v1_val_u64s().
fn alloc_v1_val_u64s(a: &c::Alloc<Validated>) -> usize {
    let fields: usize = ALLOC_V1_FIELD_BYTES.iter().enumerate()
        .filter(|&(i, _)| a.fields & 1 << i != 0)
        .map(|(_, &bytes)| bytes)
        .sum();
    (offset_of!(c::Alloc, repr.data) + fields).div_ceil(size_of::<u64>())
}

// ── Alloc keys as v4 ─────────────────────────────────────────────────────

/// Alloc key @k, of whatever version, as a current one - one that isn't an
/// alloc key, the slot of a bucket without one, as a zeroed one:
/// bch2_alloc_to_v4().
pub fn alloc_to_v4(k: BkeySC<'_>) -> c::AllocV4 {
    // A v4 by its length: fields past it are zero, as written before they
    // existed. Not by BACKPOINTERS_START - a key widened without updating
    // it has fields written past it since
    let mut a = c::AllocV4::val_copy_pad(k).unwrap_or_else(|| {
        let u = AllocUnpacked::new(k);
        let mut a = c::AllocV4::default();
        a.journal_seq_nonempty = u.journal_seq;
        a.flags                = u.need_discard.into();
        a.generation           = u.generation;
        a.oldest_gen           = u.oldest_gen;
        a.data_type            = u.data_type;
        a.dirty_sectors        = u.dirty_sectors;
        a.cached_sectors       = u.cached_sectors;
        a.io_time              = [u.read_time, u.write_time];
        a.stripe_refcount      = (u.stripe != 0).into();
        a
    });
    a.set_backpointers_start(c::BCH_ALLOC_V4_U64s as u64);
    a
}

#[c_export]
pub fn __bch2_alloc_to_v4(k: BkeySC<'_>, out: &mut c::AllocV4) {
    *out = alloc_to_v4(k);
}

impl Fs {
    /// The io clock for @rw, as a bucket's io_time holds it - never 0, which
    /// is none: bch2_current_io_time().
    pub fn current_io_time(&self, rw: usize) -> u64 {
        (self.io_clock[rw].now.load(Ordering::Relaxed) as u64 & c::LRU_TIME_MAX).max(1)
    }
}

impl<'a, 't> TransAttempt<'a, 't> {
    /// alloc_to_v4(), as a key in transaction memory to edit and update with
    /// - not queued as an update: bch2_alloc_to_v4_mut(). A v4 is grown to
    /// all of today's struct, or kept as long as it was if that's longer.
    pub fn alloc_to_v4_mut(&self, k: BkeySC<'_>) -> Result<TransBkey<'a, 't, c::AllocV4>, BchError> {
        if let Some(a) = k.arm::<c::AllocV4>() {
            let mut a = self.bkey_make_mut_noupdate(a)?;
            // Widened to all of today's struct: say so, for a kernel that
            // reads by BACKPOINTERS_START - one from before it read by length
            // would drop what's written past an older start
            a.v_mut().set_backpointers_start(c::BCH_ALLOC_V4_U64s as u64);
            return Ok(a);
        }

        let mut a = self.bkey_alloc_typed::<c::AllocV4>()?;
        a.k_mut().p = k.k.p;
        *a.v_mut() = alloc_to_v4(k);
        Ok(a)
    }

    /// The alloc key of bucket @pos, queued as its update, to edit - the
    /// update already queued, if there is one: bch2_trans_start_alloc_update().
    pub fn start_alloc_update(&self, pos: c::bpos, flags: UpdateTriggerFlags)
        -> Result<TransBkey<'a, 't, c::AllocV4>, BchError>
    {
        let mut iter = BtreeIter::new(self, c::btree_id::alloc, pos,
                                      BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
        let k = iter.peek_slot(self)?.expect("a slot always has a key");

        if self.in_mem(k.v.cast()) {
            let a = k.arm::<c::AllocV4>().expect("an alloc update, as alloc_to_v4_mut() makes one");
            // alloc_to_v4_mut() made it, all of an alloc_v4 or longer; by its
            // value, as C's container_of(): the header @k has may be a copy
            let u64s = u32::from(a.k.u64s).max((size_of::<BkeyI<c::AllocV4>>() / size_of::<u64>()) as u32);
            let key = k.v.cast::<u8>().wrapping_sub(size_of::<c::bkey>()).cast_mut();
            // SAFETY: a key in our memory, its value at @k.v, with @u64s u64s of room
            let a = unsafe { TransBkey::from_raw_room(self, key.cast(), u64s) };
            // SAFETY: all of an alloc_v4
            return Ok(unsafe { a.whole_unchecked() });
        }

        let a = self.alloc_to_v4_mut(k)?;
        self.update(&iter, &a, flags)?;
        Ok(a)
    }

    /// Device @ca's bucket counts and sectors, for a bucket going from @old
    /// to @new: bch2_alloc_key_to_dev_counters().
    pub fn alloc_key_to_dev_counters(&self, ca: &c::bch_dev, old: &c::AllocV4, new: &c::AllocV4,
                                     flags: UpdateTriggerFlags) -> Result<(), BchError> {
        let ca = core::ptr::from_ref(ca).cast_mut();
        self.result(unsafe { c::bch2_alloc_key_to_dev_counters(self.raw(), ca, old, new, flags.bits()) })
    }
}

/// An alloc_v4 key for C: in transaction memory, or an ERR_PTR.
fn alloc_v4_to_c(a: Result<TransBkey<'_, '_, c::AllocV4>, BchError>) -> *mut c::bkey_i_alloc_v4 {
    a.map_or_else(err_ptr, |a| a.as_ptr().cast())
}

#[c_export]
pub fn bch2_alloc_to_v4_mut<'t>(trans: &'t BtreeTrans<'t>, k: BkeySC<'_>) -> *mut c::bkey_i_alloc_v4 {
    // SAFETY: C calls it in an attempt
    let t = unsafe { trans.attempt_in_progress() };
    alloc_v4_to_c(t.alloc_to_v4_mut(k))
}

#[c_export]
pub fn bch2_trans_start_alloc_update<'t>(trans: &'t BtreeTrans<'t>, pos: c::bpos,
                                         flags: c::btree_iter_update_trigger_flags) -> *mut c::bkey_i_alloc_v4 {
    // SAFETY: C calls it in an attempt
    let t = unsafe { trans.attempt_in_progress() };
    alloc_v4_to_c(t.start_alloc_update(pos, UpdateTriggerFlags::from_bits_retain(flags.bits())))
}

// ── Alloc keys: validate, swab, to_text ──────────────────────────────────

fn alloc_v1_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let a = v.k.arm::<c::Alloc>().expect("an alloc key");
    let (have, want) = (v.k.k.val_bytes() / size_of::<u64>(), alloc_v1_val_u64s(a.v()));

    // allow for unknown fields
    bkey_fsck_err_on!(v, have < want, id::alloc_v1_val_size_bad, "incorrect value size ({have} < {want})")
}

fn alloc_v4_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    use c::bch_data_type as dt;

    let a = c::AllocV4::val_copy_pad(v.k).expect("an alloc_v4");
    let (want, have) = (a.u64s_noerror(), v.k.k.val_bytes() / size_of::<u64>());

    bkey_fsck_err_on!(v, want > have, id::alloc_v4_val_size_bad, "bad val size ({want} > {have})")?;

    // Backpointers stored after the value, as before they moved to the
    // backpointers btree (no_bps_in_alloc_keys): a value is read by its
    // length, where these would read as fields added since. Dropping the key
    // flags the alloc btree as having lost data, and check_allocations
    // rebuilds it.
    bkey_fsck_err_on!(v, a.nr_backpointers() != 0, id::alloc_v4_inline_backpointers,
                      "{} backpointers inline, from before they moved to the backpointers btree",
                      a.nr_backpointers())?;

    let data_type = a.alloc_data_type(a.data_type());
    bkey_fsck_err_on!(v, data_type.0 != u32::from(a.data_type), id::alloc_key_data_type_bad,
                      "invalid data type (got {} should be {})", a.data_type, data_type.0)?;

    for (rw, name) in [(READ, "read"), (WRITE, "write")] {
        bkey_fsck_err_on!(v, a.io_time[rw] > c::LRU_TIME_MAX, id::alloc_key_io_time_bad,
                          "invalid io_time[{name}]: {}, max {}", a.io_time[rw], c::LRU_TIME_MAX)?;
    }

    let (stripe, dirty, cached, refcount) = (a.stripe_sectors, a.dirty_sectors, a.cached_sectors, a.stripe_refcount);
    match a.data_type() {
        dt::BCH_DATA_free | dt::BCH_DATA_need_gc_gens | dt::BCH_DATA_need_discard =>
            bkey_fsck_err_on!(v, stripe != 0 || dirty != 0 || cached != 0 || refcount != 0,
                              id::alloc_key_empty_but_have_data,
                              "empty data type free but have data {stripe}.{dirty}.{cached} {refcount}"),
        dt::BCH_DATA_sb | dt::BCH_DATA_journal | dt::BCH_DATA_btree | dt::BCH_DATA_user | dt::BCH_DATA_parity =>
            bkey_fsck_err_on!(v, dirty == 0 && stripe == 0 && refcount == 0, id::alloc_key_dirty_sectors_0,
                              "data_type {} but dirty_sectors==0", data_type_str(a.data_type())),
        dt::BCH_DATA_cached =>
            bkey_fsck_err_on!(v, cached == 0 || dirty != 0 || stripe != 0 || refcount != 0,
                              id::alloc_key_cached_inconsistency, "data type inconsistency"),
        _ => Ok(()),
    }
}

#[c_export]
pub fn bch2_alloc_v1_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(alloc_v1_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[c_export]
pub fn bch2_alloc_v2_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    let v = BkeyValidate { fs: Fs::from_c(c), k, from };
    let a = k.arm::<c::AllocV2>().expect("an alloc_v2");
    ret_to_c(bkey_fsck_err_on!(&v, AllocUnpacked::v2(a).is_none(), id::alloc_v2_unpack_error, "unpack error"))
}

#[c_export]
pub fn bch2_alloc_v3_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    let v = BkeyValidate { fs: Fs::from_c(c), k, from };
    let a = k.arm::<c::AllocV3>().expect("an alloc_v3");
    ret_to_c(bkey_fsck_err_on!(&v, AllocUnpacked::v3(a).is_none(), id::alloc_v3_unpack_error, "unpack error"))
}

#[c_export]
pub fn bch2_alloc_v4_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(alloc_v4_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

/// Each field in the value's bytes byte swapped - only those the value has:
/// one from before the struct grew ends short of the later fields.
#[c_export]
pub fn bch2_alloc_v4_swab(_c: Option<&Fs>, mut k: BkeyS<'_>) {
    fn swab<const N: usize>(v: &mut [u8], at: usize) {
        if let Some(b) = v.get_mut(at..at + N) {
            b.reverse();
        }
    }
    let v = k.val_bytes_mut();

    swab::<8>(v, offset_of!(c::AllocV4, repr.journal_seq_nonempty));
    swab::<8>(v, offset_of!(c::AllocV4, repr.journal_seq_empty));
    swab::<4>(v, offset_of!(c::AllocV4, repr.flags));
    swab::<4>(v, offset_of!(c::AllocV4, repr.dirty_sectors));
    swab::<4>(v, offset_of!(c::AllocV4, repr.cached_sectors));
    swab::<8>(v, offset_of!(c::AllocV4, repr.io_time));
    swab::<8>(v, offset_of!(c::AllocV4, repr.io_time) + size_of::<u64>());
    swab::<4>(v, offset_of!(c::AllocV4, repr.stripe_refcount));
    swab::<4>(v, offset_of!(c::AllocV4, repr.nr_external_backpointers));
    swab::<4>(v, offset_of!(c::AllocV4, repr.stripe_sectors));
}

/// @a, read from alloc key @k: what's printed of a v4's later fields depends
/// on @k's length, whatever its version.
fn alloc_v4_to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>, a: &c::AllocV4) {
    let has = |at: usize, len: usize| k.k.val_bytes() >= at + len;
    let ca = fs.and_then(|fs| fs.dev_get(k.k.p.inode as u32));

    prt!(out, "\ngen {} oldest_gen {} data_type ", a.generation, a.oldest_gen);
    prt_data_type(out, a.data_type());
    prt!(out, "\njournal_seq_nonempty {}\n", { a.journal_seq_nonempty });
    if has(offset_of!(c::AllocV4, repr.journal_seq_empty), size_of::<u64>()) {
        prt!(out, "journal_seq_empty    {}\n", { a.journal_seq_empty });
    }
    prt!(out, "need_discard         {}\n", u8::from(a.need_discard()));
    prt!(out, "need_inc_gen         {}\n", u8::from(a.need_inc_gen()));
    prt!(out, "dirty_sectors        {}\n", { a.dirty_sectors });
    if has(offset_of!(c::AllocV4, repr.stripe_sectors), size_of::<u32>()) {
        prt!(out, "stripe_sectors       {}\n", { a.stripe_sectors });
    }
    prt!(out, "cached_sectors       {}\n", { a.cached_sectors });
    prt!(out, "stripe_refcount      {}\n", { a.stripe_refcount });
    prt!(out, "io_time[READ]        {}\n", { a.io_time[READ] });
    prt!(out, "io_time[WRITE]       {}\n", { a.io_time[WRITE] });
    if let Some(ca) = &ca {
        prt!(out, "fragmentation     {}\n", a.lru_idx_fragmentation(ca));
    }
    prt!(out, "bp_start          {}\n", a.backpointers_start());
}

#[cold]
#[c_export]
pub fn bch2_alloc_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>, k: BkeySC<'_>) {
    alloc_v4_to_text(out, c.map(Fs::from_c), k, &alloc_to_v4(k));
}

#[cold]
#[c_export]
pub fn bch2_alloc_v4_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>, k: BkeySC<'_>) {
    let a = c::AllocV4::val_copy_pad(k).expect("an alloc_v4");
    alloc_v4_to_text(out, c.map(Fs::from_c), k, &a);
}

impl c::AllocV4 {
    fn data_type(&self) -> data_type {
        data_type_from_u8(self.data_type)
    }

    /// bch2_bucket_sectors_dirty(): u32s added as C's, wrapping.
    fn sectors_dirty(&self) -> i64 {
        i64::from(self.stripe_sectors.wrapping_add(self.dirty_sectors))
    }

    /// bch2_bucket_sectors().
    fn sectors(&self) -> i64 {
        if self.data_type() == data_type::cached {
            i64::from(self.cached_sectors)
        } else {
            self.sectors_dirty()
        }
    }

    /// The sectors of @ca's bucket that aren't in use, a full bucket of them if
    /// it's empty but not free: bch2_bucket_sectors_fragmented(), which takes
    /// the sectors as an int.
    fn sectors_fragmented(&self, ca: &c::bch_dev) -> i64 {
        let d = self.sectors() as i32;
        let bucket_size = i32::from(ca.mi.bucket_size);

        i64::from(if d != 0 {
            (bucket_size - d).max(0)
        } else if !data_type_is_empty(self.data_type()) {
            bucket_size
        } else {
            0
        })
    }

    /// Its time in its device's read lru, 0 for none - only cached data is
    /// in it: alloc_lru_idx_read().
    pub fn lru_idx_read(&self) -> u64 {
        if self.data_type() == data_type::cached {
            self.io_time[READ] & c::LRU_TIME_MAX
        } else {
            0
        }
    }

    /// Its time in @ca's fragmentation lru, 0 for none - how full it is, as a
    /// fraction of 1 << 31, for movable data with room it isn't using:
    /// alloc_lru_idx_fragmentation().
    pub fn lru_idx_fragmentation(&self, ca: &c::bch_dev) -> u64 {
        if self.data_type >= c::BCH_DATA_NR as u8 ||
           !data_type_movable(self.data_type()) ||
           self.sectors_fragmented(ca) == 0 {
            return 0;
        }

        // on a corrupt fs, more dirty sectors than the bucket has
        let bucket_size = u64::from(ca.mi.bucket_size);
        let d = self.sectors_dirty().min(bucket_size as i64) as u64;
        d * (1 << 31) / bucket_size
    }

    /// Its value's size in u64s by its own account: from where its
    /// backpointers start - v4's original size, if that's unset - and any
    /// inline: alloc_v4_u64s_noerror().
    fn u64s_noerror(&self) -> usize {
        let start = match self.backpointers_start() {
            0 => u64::from(c::BCH_ALLOC_V4_U64s_V0),
            s => s,
        };
        (start + self.nr_backpointers() * (size_of::<c::Backpointer>() / size_of::<u64>()) as u64) as usize
    }

    /// How far its generation is past its oldest pointer's, wrapping:
    /// alloc_gc_gen().
    pub fn gc_gen(&self) -> u8 {
        self.generation.wrapping_sub(self.oldest_gen)
    }

    /// Its type, from what it holds: @t is what's being added to it, or
    /// need_discard for one just emptied.
    pub fn alloc_data_type(&self, t: data_type) -> data_type {
        if self.data_type() == data_type::multiple {
            data_type::multiple
        } else if self.stripe_refcount != 0 {
            if t == data_type::parity { t } else { data_type::stripe }
        } else if self.sectors_dirty() != 0 {
            bucket_data_type(t)
        } else if self.cached_sectors != 0 {
            data_type::cached
        } else if t == data_type::need_discard {
            data_type::need_discard
        } else if u32::from(self.gc_gen()) >= c::BUCKET_GC_GEN_MAX {
            data_type::need_gc_gens
        } else {
            data_type::free
        }
    }

    pub fn set_data_type(&mut self, t: data_type) {
        self.data_type = self.alloc_data_type(t).0 as u8;
    }
}

// ── Positions in the btrees an alloc key indexes into ────────────────────
//
// need_discard, freespace and bucket_gens are indexes of the alloc btree,
// each with its own encoding of a bucket's position - background.h's
// inlines, for Rust: positions as their own types, converted to and from
// bpos at the btree.

/// A bucket's entry in the need_discard btree: the journal seq it became
/// empty at, then the bucket, as bucket_to_u64().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeedDiscardPos {
    pub journal_seq_empty: u64,
    pub bucket:            c::bpos,
}

impl NeedDiscardPos {
    /// Where bucket @bucket, alloc key @a, belongs.
    pub fn new(bucket: c::bpos, a: &c::AllocV4) -> Self {
        Self { journal_seq_empty: a.journal_seq_empty, bucket }
    }

    pub fn from_bpos(p: c::bpos) -> Self {
        Self { journal_seq_empty: p.inode, bucket: u64_to_bucket(p.offset) }
    }

    pub fn to_bpos(self) -> c::bpos {
        pos(self.journal_seq_empty, bucket_to_u64(self.bucket))
    }
}

/// Where in a free bucket's offset, in the freespace btree, the high bits of
/// its gc gen go.
const FREESPACE_GENBITS_SHIFT: u32 = 56;

/// A free bucket's place in the freespace btree: the bucket, with the high
/// bits of its gc gen in the top bits of the offset - alloc_freespace_pos().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreespacePos {
    pub bucket:  c::bpos,
    /// The bucket's gc gen >> 4: alloc_freespace_genbits() >> 56. Read from
    /// the btree, whatever the top 8 bits hold.
    pub genbits: u8,
}

impl FreespacePos {
    /// Where bucket @bucket, alloc key @a, belongs.
    pub fn new(bucket: c::bpos, a: &c::AllocV4) -> Self {
        Self { bucket, genbits: a.gc_gen() >> 4 }
    }

    pub fn from_bpos(p: c::bpos) -> Self {
        Self {
            bucket:  pos(p.inode, p.offset & ((1 << FREESPACE_GENBITS_SHIFT) - 1)),
            genbits: (p.offset >> FREESPACE_GENBITS_SHIFT) as u8,
        }
    }

    pub fn to_bpos(self) -> c::bpos {
        pos(self.bucket.inode, self.bucket.offset | u64::from(self.genbits) << FREESPACE_GENBITS_SHIFT)
    }
}

/// Where a bucket's generation is in the bucket_gens btree: the key - one per
/// KEY_TYPE_BUCKET_GENS_NR buckets - and its index in the key's gens[]:
/// alloc_gens_pos().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketGensPos {
    pub key: c::bpos,
    /// < KEY_TYPE_BUCKET_GENS_NR
    pub idx: usize,
}

impl BucketGensPos {
    pub fn new(bucket: c::bpos) -> Self {
        Self {
            key: pos(bucket.inode, bucket.offset >> c::KEY_TYPE_BUCKET_GENS_BITS),
            idx: (bucket.offset & u64::from(c::KEY_TYPE_BUCKET_GENS_MASK)) as usize,
        }
    }

    /// The bucket it's the gen of: bucket_gens_pos_to_alloc().
    pub fn bucket(self) -> c::bpos {
        pos(self.key.inode, self.key.offset << c::KEY_TYPE_BUCKET_GENS_BITS | self.idx as u64)
    }

    /// The bucket's gen in @k, its bucket_gens key - 0 if it hasn't one:
    /// alloc_gen().
    pub fn bucket_gen(self, k: BkeySC<'_>) -> u8 {
        k.arm::<c::BucketGens>().map_or(0, |g| g.v().gens[self.idx])
    }
}

// ── bucket_gens keys ─────────────────────────────────────────────────────

fn bucket_gens_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let (have, want) = (v.k.k.val_bytes(), size_of::<c::BucketGens>());
    bkey_fsck_err_on!(v, have != want, id::bucket_gens_val_size_bad, "bad val size ({have} != {want})")
}

#[c_export]
pub fn bch2_bucket_gens_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(bucket_gens_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_bucket_gens_to_text(out: &mut Printbuf, _c: Option<&Opaque<c::bch_fs>>, k: BkeySC<'_>) {
    if let Some(g) = k.arm::<c::BucketGens>() {
        for (i, gen) in g.v().gens.iter().enumerate() {
            prt!(out, "{}{gen}", if i != 0 { " " } else { "" });
        }
    }
}

/// The bucket_gens btree, made from the alloc btree: for the upgrade that
/// added it.
fn bucket_gens_init(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);
    // the key being filled, and its position
    let mut g: Option<(c::bpos, c::BucketGens)> = None;

    fn insert(t: &TransAttempt<'_, '_>, (p, gens): (c::bpos, c::BucketGens)) -> Result<(), BchError> {
        let mut k = t.bkey_alloc_typed::<c::BucketGens>()?;
        k.k_mut().p = p;
        *k.v_mut() = gens;
        t.insert(c::btree_id::bucket_gens, k, UpdateTriggerFlags::empty())
    }

    let mut iter = BtreeIter::new(&trans, c::btree_id::alloc, POS_MIN, BtreeIterFlags::PREFETCH);
    iter.for_each(&trans, |t, _, k| {
        // Not a fsck error: check_alloc_key(), later, checks it
        if fs.dev_bucket_tryget_noerror(k.k.p).is_none() {
            return Ok(());
        }

        let gens_pos = BucketGensPos::new(k.k.p);
        // cleared once committed: a restart redoes this key, the full one with it
        if let Some(full) = g.filter(|(p, _)| *p != gens_pos.key) {
            insert(t, full)?;
            t.commit(None, CommitFlags::NO_ENOSPC)?;
            g = None;
        }

        let (_, gens) = g.get_or_insert_with(|| (gens_pos.key, c::BucketGens::default()));
        gens.gens[gens_pos.idx] = alloc_to_v4(k).generation;
        Ok(())
    })?;

    match g {
        Some(last) => commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| insert(t, last)),
        None       => Ok(()),
    }
}

crate::recovery_pass!(bch2_bucket_gens_init => bucket_gens_init);

/// Each device's bucket generations, in memory: from the bucket_gens btree,
/// or before it existed, the alloc btree.
fn alloc_read(fs: &Fs) -> Result<(), BchError> {
    let _state = fs.state_lock_read();
    let trans = btree_trans!(fs);
    let mut ca = None;

    let ver = fs.version_upgrade_complete().0;
    if ver >= c::bcachefs_metadata_version::bcachefs_metadata_version_bucket_gens.0 {
        let mut iter = BtreeIter::new(&trans, c::btree_id::bucket_gens, POS_MIN, BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |_, _, k| {
            let Some(g) = k.arm::<c::BucketGens>() else { return Ok(LoopFlow::Continue) };

            // Not a fsck error: check_bucket_gens_key(), later, checks it
            fs.dev_iterate(&mut ca, k.k.p.inode as u32);
            let Some(ca) = &ca else { return Ok(LoopFlow::ContinueFrom(pos(k.k.p.inode + 1, 0))) };

            let start = BucketGensPos { key: k.k.p, idx: 0 }.bucket().offset;
            let end = BucketGensPos { key: k.k.p.nosnap_successor(), idx: 0 }.bucket().offset;
            let rcu = read_lock();
            for b in start.max(ca.mi.first_bucket.into())..end.min(ca.mi.nbuckets) {
                ca.bucket_gen_ref(b, &rcu).expect("one of the device's buckets")
                    .store(g.v().gens[(b & u64::from(c::KEY_TYPE_BUCKET_GENS_MASK)) as usize], Ordering::Relaxed);
            }
            Ok(LoopFlow::Continue)
        })
    } else {
        let mut iter = BtreeIter::new(&trans, c::btree_id::alloc, POS_MIN, BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |_, _, k| {
            // Not a fsck error: check_alloc_key(), later, checks it
            fs.dev_iterate(&mut ca, k.k.p.inode as u32);
            let Some(ca) = &ca else { return Ok(LoopFlow::ContinueFrom(pos(k.k.p.inode + 1, 0))) };

            if k.k.p.offset < ca.mi.first_bucket.into() {
                return Ok(LoopFlow::ContinueFrom(pos(k.k.p.inode, ca.mi.first_bucket.into())));
            }
            if k.k.p.offset >= ca.mi.nbuckets {
                return Ok(LoopFlow::ContinueFrom(pos(k.k.p.inode + 1, 0)));
            }

            ca.bucket_gen_ref(k.k.p.offset, &read_lock()).expect("one of the device's buckets")
                .store(alloc_to_v4(k).generation, Ordering::Relaxed);
            Ok(LoopFlow::Continue)
        })
    }
}

crate::recovery_pass!(bch2_alloc_read => alloc_read);

#[cfg(test)]
mod tests {
    //! Unpacking alloc keys from before v4: nothing writes those any more, and
    //! only an upgrade from a filesystem old enough to have them reads them -
    //! nothing else would notice them wrong.

    use super::*;

    fn varints(vals: &[u64]) -> ([u8; 64], usize) {
        let mut out = [0u8; 64];
        let n = vals.iter().fold(0, |n, &v| n + varint::encode(&mut out[n..], v));
        (out, n)
    }

    #[test]
    fn alloc_v1_fields() {
        // read_time, data_type and stripe present: bits 0, 2, 6
        let data = [0x34, 0x12, 0x05, 0x78, 0x56, 0x34, 0x12];
        let u = AllocUnpacked::default().v1_fields(1 << 0 | 1 << 2 | 1 << 6, &data);

        assert_eq!((u.read_time, u.write_time, u.data_type, u.dirty_sectors, u.stripe),
                   (0x1234, 0, 5, 0, 0x1234_5678));

        // cut short - to_text prints what validate refuses
        let u = AllocUnpacked::default().v1_fields(1 << 0 | 1 << 6, &data[..4]);
        assert_eq!((u.read_time, u.stripe), (0x1234, 0));
    }

    #[test]
    fn alloc_varint_fields() {
        let (d, n) = varints(&[1 << 40, 7, 100, 200, 3]);
        let u = AllocUnpacked::default().varint_fields(&d[..n], 5).expect("fields that fit");
        assert_eq!((u.read_time, u.write_time, u.dirty_sectors, u.cached_sectors, u.stripe, u.stripe_redundancy),
                   (1 << 40, 7, 100, 200, 3, 0));

        // more fields than this version knows: the rest ignored
        let (d, n) = varints(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(AllocUnpacked::default().varint_fields(&d[..n], 8).is_some());

        // dirty_sectors is 32 bits
        let (d, n) = varints(&[0, 0, 1 << 32]);
        assert!(AllocUnpacked::default().varint_fields(&d[..n], 3).is_none());

        // a varint cut short
        let (d, n) = varints(&[0, 0, 1 << 20]);
        assert!(AllocUnpacked::default().varint_fields(&d[..n - 1], 3).is_none());
    }
}
