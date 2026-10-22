// SPDX-License-Identifier: GPL-2.0

//! Stripe keys - ec/trigger.c's, as it converts: their bkey methods, and
//! where in a stripe's value each of its parts is.
//!
//! A stripe's value is its header, then its blocks' pointers, ptrs[nr_blocks]
//! - the extent entries data/ptrs.rs walks - then the checksums, a 2D array
//! of [block][checksum block] of the type's size, then a u16 sector count per
//! block. So everything after the pointers is found through csum_type: with
//! one this version doesn't know, a stripe stays readable but its block counts
//! can't be found - validate passes it on purpose, and only what would update
//! it stops.

use crate::btree::bkey::{bkey_eq, bpos_gt, pos, BkeySC, BkeyV, BkeyVMut, POS_MIN};
use crate::c;
use crate::data::ptrs::{bkey_ptrs_validate, extent_ptr_to_text};
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use crate::util::rcu::read_lock;
use crate::util::vstructs::{cast_plain, ArmRef, Validated};
use crate::util::Printbuf;
use crate::bkey_fsck_err_on;
use core::ffi::c_int;
use core::mem::{offset_of, size_of};
use cstruct_macros::{c_const, c_export};

/// Where a stripe's pointers start: sizeof(struct bch_stripe).
const STRIPE_PTRS: usize = offset_of!(c::Stripe, repr.ptrs);

/// The offsets of a stripe's parts in its value: ec/trigger.h's inlines. In
/// C's unsigned arithmetic; csum_granularity_bits is under 31, as validate
/// checks first.
impl c::Stripe {
    /// stripe_csums_per_device().
    fn csums_per_device(&self) -> u32 {
        u32::from(self.sectors.get()).div_ceil(1 << self.csum_granularity_bits)
    }

    /// A checksum's size - for a type this version doesn't know, the widest
    /// it does: stripe_csum_bytes().
    fn csum_bytes(&self) -> u32 {
        c::bch_csum_type(self.csum_type.into()).bytes().unwrap_or(16)
    }

    /// stripe_csum_offset().
    fn csum_offset(&self, dev: u32, csum_idx: u32) -> u32 {
        STRIPE_PTRS as u32 + size_of::<c::bch_extent_ptr>() as u32 * u32::from(self.nr_blocks) +
            (dev * self.csums_per_device() + csum_idx) * self.csum_bytes()
    }

    /// stripe_blockcount_offset().
    fn blockcount_offset(&self, idx: u32) -> u32 {
        self.csum_offset(self.nr_blocks.into(), 0) + size_of::<u16>() as u32 * idx
    }

    /// stripe_val_u64s().
    fn val_u64s(&self) -> u32 {
        self.blockcount_offset(self.nr_blocks.into()).div_ceil(8)
    }

    /// Block @idx's sector count, in @val, this stripe's value - if it's long
    /// enough to hold it: stripe_blockcount_get().
    fn blockcount(&self, val: &[u8], idx: u32) -> Option<u16> {
        let at = self.blockcount_offset(idx) as usize;
        val.get(at..at + 2).map(|n| u16::from_le_bytes([n[0], n[1]]))
    }

    /// Its time in the stripe lru, @val its value: 0, not in it, if it has
    /// no room for new data; STRIPE_LRU_POS_EMPTY if it has no data, for the
    /// stripe_delete worker; else earlier the more it can take - empty data
    /// blocks, and blocks it could be widened by - to be reused first.
    ///
    /// Written by the stripe trigger, checked by check_lrus. nr_data is
    /// unsigned, as C's: nr_redundant past nr_blocks is damage validate
    /// doesn't catch, and only blocks the value has are counted.
    pub fn lru_pos(&self, val: &[u8]) -> u64 {
        let nr_blocks = u32::from(self.nr_blocks);
        let nr_data = nr_blocks.wrapping_sub(self.nr_redundant.into());
        let blocks_empty = (0..nr_data.min(nr_blocks))
            .filter(|&i| self.blockcount(val, i) == Some(0))
            .count() as u32;

        if blocks_empty == nr_data {
            return STRIPE_LRU_POS_EMPTY;
        }

        match blocks_empty + u32::from(self.can_widen()) {
            0        => 0,
            reusable => c::LRU_TIME_MAX - u64::from(reusable),
        }
    }
}

c_const! {
    pub const STRIPE_LRU_POS_EMPTY: u64 = 1;
}

/// Stripe::lru_pos(), for C.
///
/// XXX: "validate passed" isn't true of the new key in the stripe trigger:
/// without CONFIG_BCACHEFS_DEBUG, keys are validated when their btree node or
/// journal entry is written, after the triggers - so a stripe with
/// csum_granularity_bits >= 32 would panic here, in val_u64s()'s shift, where
/// C's was UB and went on. The same is true of any trigger trusting its new
/// key's Validated. The fix is proving we never make a key validate would
/// reject - formal verification - not checking for it here.
///
/// # Safety
/// @s is NULL - no stripe, 0 - or a stripe value validate passed: as long as
/// its header says.
#[c_export]
pub unsafe extern "C" fn stripe_lru_pos(s: *const c::Stripe) -> u64 {
    // SAFETY: the caller's
    let Some(s) = (unsafe { s.as_ref() }) else { return 0 };
    let val = unsafe { core::slice::from_raw_parts((s as *const c::Stripe).cast::<u8>(),
                                                   s.val_u64s() as usize * 8) };
    s.lru_pos(val)
}

/// Block @i's pointer, in stripe value @val - if it's long enough to hold it,
/// as validate checks it is.
fn block_ptr(val: &[u8], i: u32) -> Option<&c::bch_extent_ptr> {
    let at = STRIPE_PTRS + size_of::<c::bch_extent_ptr>() * i as usize;
    val.get(at..at + size_of::<c::bch_extent_ptr>()).and_then(cast_plain)
}

/// Each block's pointer, as far as the value goes.
pub fn stripe_block_ptrs<'a>(s: BkeySC<'a, c::Stripe<Validated>>) -> impl Iterator<Item = &'a c::bch_extent_ptr> {
    let val = s.val_bytes();
    (0..u32::from(s.v().nr_blocks)).map_while(move |i| block_ptr(val, i))
}

/// Whether @p is the data block its stripe_ptr names in @s:
/// bch2_ptr_matches_stripe(). A block past the value matches nothing.
pub fn ptr_matches_stripe(s: BkeySC<'_, c::Stripe<Validated>>, p: &c::extent_ptr_decoded) -> bool {
    assert!(p.has_ec, "matching a pointer without a stripe against one");

    let v = s.v();
    // unsigned, as C's: nr_redundant past nr_blocks is damage
    let nr_data = u32::from(v.nr_blocks).wrapping_sub(v.nr_redundant.into());
    let block = u32::from(p.ec.block());
    if block >= nr_data {
        return false;
    }
    let Some(sp) = block_ptr(s.val_bytes(), block) else { return false };

    ptr_in_block(sp, &p.ptr, u64::from(v.sectors.get()))
}

/// ptr_matches_stripe(), against gc's copy of the stripe, locked.
pub fn ptr_matches_stripe_m(m: &c::GcStripeData, p: &c::extent_ptr_decoded) -> bool {
    assert!(p.has_ec, "matching a pointer without a stripe against one");

    // unsigned, as C's
    let nr_data = u32::from(m.nr_blocks).wrapping_sub(m.nr_redundant.into());
    let block = u32::from(p.ec.block());
    block < nr_data && ptr_in_block(&m.ptrs[block as usize], &p.ptr, u64::from(m.sectors))
}

/// Whether @data_ptr is in the block of @sectors at @stripe_ptr.
fn ptr_in_block(stripe_ptr: &c::bch_extent_ptr, data_ptr: &c::bch_extent_ptr, sectors: u64) -> bool {
    let invalid = c::BCH_SB_MEMBER_INVALID as u8;
    (data_ptr.dev() == stripe_ptr.dev() || data_ptr.dev() == invalid || stripe_ptr.dev() == invalid) &&
        data_ptr.generation() == stripe_ptr.generation() &&
        data_ptr.offset() >= stripe_ptr.offset() &&
        data_ptr.offset() < stripe_ptr.offset() + sectors
}

// ── gc's stripes ─────────────────────────────────────────────────────────

impl c::bch_fs_ec {
    /// gc's copy of stripe @idx, if it has one.
    pub fn gc_stripe(&self, idx: u64) -> Option<&c::gc_stripe> {
        // SAFETY: an entry - a Lock, all of whose state is behind its lock
        self.gc_stripes.get_raw(idx as usize).map(|m| unsafe { m.as_ref() })
    }

    /// gc_stripe(), allocated - zeroed, not alive - if it wasn't.
    pub fn gc_stripe_alloc(&self, idx: u64, gfp: c::gfp_t) -> Result<&c::gc_stripe, AllocError> {
        self.gc_stripes.prealloc(idx as usize, gfp)?;
        Ok(self.gc_stripe(idx).expect("an entry, allocated"))
    }
}

#[c_export]
pub fn bch2_gc_stripe_get(c: &Opaque<c::bch_fs>, idx: u64) -> *mut c::gc_stripe {
    Fs::from_c(c).ec.gc_stripe(idx).map_or(core::ptr::null_mut(), |m| core::ptr::from_ref(m).cast_mut())
}

/// NULL if it couldn't be allocated.
#[c_export]
pub fn bch2_gc_stripe_alloc(c: &Opaque<c::bch_fs>, idx: u64, gfp: c::gfp_t) -> *mut c::gc_stripe {
    Fs::from_c(c).ec.gc_stripe_alloc(idx, gfp).map_or(core::ptr::null_mut(), |m| core::ptr::from_ref(m).cast_mut())
}

/// # Safety
/// gc is done with them: gc.pos is back to not_running. Not fenced from a
/// commit's gc trigger that saw it running - as reflink's gc table isn't:
/// see ReflinkGc.
#[c_export]
pub unsafe fn bch2_gc_stripes_free(c: &Opaque<c::bch_fs>) {
    unsafe { Fs::from_c(c).ec.gc_stripes.free() }
}

/// @sectors added to block @idx's count in stripe @k, wrapping to a u16 as C's.
pub fn stripe_blockcount_add(k: &mut impl BkeyVMut, idx: u32, sectors: i64) {
    let at = stripe_header_mut(k).blockcount_offset(idx) as usize;
    let n = &mut k.val_bytes_mut()[at..at + 2];
    let v = (i64::from(u16::from_le_bytes([n[0], n[1]])) + sectors) as u16;
    n.copy_from_slice(&v.to_le_bytes());
}

impl c::Stripe {
    /// A checksum type this version doesn't know is an error, not a guess.
    pub fn csum_type_check(&self, fs: &Fs) -> Result<(), BchError> {
        if u32::from(self.csum_type) >= c::bch_csum_type::BCH_CSUM_NR.0 {
            return fs.throw(bch_errcode::BCH_ERR_stripe_unknown_csum_type);
        }
        Ok(())
    }
}

/// Stripe @k's header, the sized part of its value, to edit in place: the
/// value is a DST, and its pointers and checksums after the header aren't
/// touched.
pub fn stripe_header_mut(k: &mut impl BkeyVMut) -> &mut c::Stripe {
    <c::Stripe as ArmRef>::arm_mut(k.val_bytes_mut()).expect("a stripe's header")
}

/// How many data blocks @nr_devs devices could give a stripe with
/// @nr_redundant parity blocks - the pool's widening target - capped at
/// BCH_BKEY_PTRS_MAX, and at @max_data_blocks if that's set (the
/// ec_max_data_blocks option): stripe_widen_target_nr_data().
pub fn stripe_widen_target_nr_data(nr_devs: u32, nr_redundant: u32, max_data_blocks: u32) -> u32 {
    if nr_devs <= nr_redundant {
        return 0;
    }
    // unsigned, as C's
    let t = nr_devs.min(c::BCH_BKEY_PTRS_MAX).wrapping_sub(nr_redundant);
    if max_data_blocks != 0 { t.min(max_data_blocks) } else { t }
}

/// What a stripe's can_widen holds: target - actual data blocks, clamped to
/// its three bits - stripe_widen_value().
pub fn stripe_widen_value(target_nr_data: u32, actual_nr_data: u32) -> u8 {
    (target_nr_data as i32).wrapping_sub(actual_nr_data as i32).clamp(0, 7) as u8
}

fn stripe_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let s = v.k.arm::<c::Stripe>().expect("a stripe").v();

    // checked before val_u64s(), which shifts by it
    bkey_fsck_err_on!(v, s.csum_granularity_bits >= 31, id::stripe_csum_granularity_bad,
                      "invalid csum granularity ({} >= 31)", s.csum_granularity_bits)?;

    bkey_fsck_err_on!(v, bkey_eq(v.k.k.p, POS_MIN) || bpos_gt(v.k.k.p, pos(0, u32::MAX.into())),
                      id::stripe_pos_bad, "stripe at bad pos")?;

    let val_u64s = v.k.val_bytes().len() / 8;
    bkey_fsck_err_on!(v, val_u64s < s.val_u64s() as usize, id::stripe_val_size_bad,
                      "incorrect value size ({} < {})", val_u64s, s.val_u64s())?;

    bkey_fsck_err_on!(v, s.sectors.get() == 0, id::stripe_sectors_zero, "invalid sectors zero")?;

    bkey_ptrs_validate(v)
}

#[c_export]
pub fn bch2_stripe_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> c_int
{
    ret_to_c(stripe_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

/// Its header, then a line per block - each block's pointer, and its sector
/// count where it can be found - as far as the value goes: this prints keys
/// validate rejected.
#[cold]
#[c_export]
pub fn bch2_stripe_to_text(out: &mut Printbuf, c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let fs = Fs::from_c(c);
    // the header's there - bch2_val_to_text() checks min_val_size, all of it,
    // before calling this - but not what's after it
    let s = k.arm::<c::Stripe>().expect("a stripe").v();
    let val = k.val_bytes();
    // unsigned, as C's - nr_redundant > nr_blocks is damage, printed as is
    let nr_data = u32::from(s.nr_blocks).wrapping_sub(s.nr_redundant.into());

    write!(out, "algo {} sectors {} blocks {}:{} csum {} gran ",
           s.algorithm(), s.sectors.get(), nr_data, s.nr_redundant, c::bch_csum_type(s.csum_type.into()));
    match 1u64.checked_shl(s.csum_granularity_bits.into()) {
        Some(gran) => write!(out, "{gran}"),
        None => write!(out, "(invalid shift {})", s.csum_granularity_bits),
    }

    write!(out, " label=");
    if s.disk_label != 0 {
        fs.disk_path_to_text(out, u32::from(s.disk_label) - 1);
    } else {
        write!(out, "(none)");
    }

    if s.needs_reconcile() != 0 {
        write!(out, " needs_reconcile");
    }
    if s.can_widen() != 0 {
        write!(out, " can_widen={}", s.can_widen());
    }

    let mut out = out.indent(2);
    let mut out = out.atomic();
    let rcu = read_lock();

    let counts_found = s.csum_type < c::bch_csum_type::BCH_CSUM_NR.0 as u8 && s.csum_granularity_bits < 31;

    for i in 0..u32::from(s.nr_blocks) {
        let Some(ptr) = block_ptr(val, i) else { break };

        out.newline();
        extent_ptr_to_text(&mut out, Some(fs), ptr, &rcu);

        if counts_found {
            if let Some(n) = s.blockcount(val, i) {
                write!(out, "#{n}");
            }
        }
    }
}
