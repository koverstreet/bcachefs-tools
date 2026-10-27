// SPDX-License-Identifier: GPL-2.0

//! The stripe key - a stripe's value, and its bkey methods but the trigger
//! (trigger.rs's): where in the value each of its parts is, matching an
//! extent's pointer to its block, widening, validate and to_text; and
//! StripeKey, a key with room for any stripe's value.
//!
//! A stripe's value is its header, then its blocks' pointers, ptrs[nr_blocks]
//! - the extent entries data/ptrs.rs walks - then the checksums, a 2D array
//! of [block][checksum block] of the type's size, then a u16 sector count per
//! block. So everything after the pointers is found through csum_type: with
//! one this version doesn't know, a stripe stays readable but its block counts
//! can't be found - validate passes it on purpose, and only what would update
//! it stops.
//!
//! The key is whatever holds it - a BkeyBuf a reader copied it into, or for a
//! stripe being made, a StripeKey: a key with room for all of its value,
//! edited as its blocks are allocated. What only reads a key takes it
//! borrowed, as a BkeySC.

use crate::bkey_fsck_err_on;
use crate::btree::bkey::{bkey_eq, bpos_gt, pos, BkeyI, BkeySC, BkeyV, POS_MIN};
use crate::c;
use crate::data::ptrs::{bkey_ptrs_validate, extent_ptr_to_text};
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::types::Le16;
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::alloc::KVVec;
use crate::util::ffi::Opaque;
use crate::util::rcu::read_lock;
use crate::util::vstructs::{cast_mut, cast_plain, cast_plain_mut, cast_plain_slice, cast_plain_slice_mut, plain_bytes,
                            plain_bytes_mut, Validated};
use crate::util::Printbuf;
use arrayvec::ArrayVec;
use core::ffi::c_int;
use core::mem::{offset_of, size_of};
use core::ops::Range;
use cstruct_macros::{c_const, c_export};

/// The most blocks a stripe has: a pointer each, of the most a key holds.
pub(crate) const MAX_BLOCKS: usize = c::BCH_BKEY_PTRS_MAX as usize;

/// Where a stripe's pointers start: sizeof(struct bch_stripe).
const STRIPE_PTRS: usize = offset_of!(c::Stripe, repr.ptrs);

/// The header: its shape, and the sizes of the sections after it - ec/trigger.h's
/// inlines. In C's unsigned arithmetic: nr_redundant past nr_blocks is damage
/// validate doesn't catch, and wraps nr_data; csum_granularity_bits is under 31,
/// as validate checks first. Offsets are into the value's tail, after the header.
impl c::Stripe {
    pub fn nr_data(&self) -> u32 {
        u32::from(self.nr_blocks).wrapping_sub(self.nr_redundant.into())
    }

    /// A checksum type this version doesn't know is an error, not a guess.
    pub fn csum_type_check(&self, fs: &Fs) -> Result<(), BchError> {
        if u32::from(self.csum_type) >= c::bch_csum_type::BCH_CSUM_NR.0 {
            return fs.throw(bch_errcode::BCH_ERR_stripe_unknown_csum_type);
        }
        Ok(())
    }

    /// The can_widen it should have, with @nr_devs RW members in its disk
    /// label and ec_max_data_blocks @max_data_blocks: how many more data
    /// blocks they'd let it have.
    pub fn want_can_widen(&self, nr_devs: u32, max_data_blocks: u32) -> u8 {
        stripe_widen_value(stripe_widen_target_nr_data(nr_devs, self.nr_redundant.into(), max_data_blocks),
                           self.nr_data())
    }

    /// stripe_csums_per_device().
    pub(crate) fn csums_per_device(&self) -> u32 {
        u32::from(self.sectors.get()).div_ceil(1 << self.csum_granularity_bits)
    }

    /// A checksum's size - None for a type this version doesn't know:
    /// stripe_csum_bytes().
    fn csum_bytes(&self) -> Option<u32> {
        c::bch_csum_type(self.csum_type.into()).bytes()
    }

    fn ptrs_bytes(&self) -> usize {
        usize::from(self.nr_blocks) * size_of::<c::bch_extent_ptr>()
    }

    fn csums_at(&self) -> Option<Range<usize>> {
        let start = self.ptrs_bytes();
        Some(start..start + (u32::from(self.nr_blocks) * self.csums_per_device() * self.csum_bytes()?) as usize)
    }

    fn blockcounts_at(&self) -> Option<Range<usize>> {
        let start = self.csums_at()?.end;
        Some(start..start + usize::from(self.nr_blocks) * size_of::<u16>())
    }

    /// stripe_val_u64s(), as validate has it: for a checksum type this version
    /// doesn't know, as though it were 16 bytes wide, the widest it knows.
    pub(crate) fn val_u64s(&self) -> u32 {
        let csums = u32::from(self.nr_blocks) * self.csums_per_device() * self.csum_bytes().unwrap_or(16);
        (STRIPE_PTRS as u32 + self.ptrs_bytes() as u32 + csums + u32::from(self.nr_blocks) * 2).div_ceil(8)
    }
}

/// A stripe's sections: its blocks' pointers, the checksums, a 2D array of
/// [block][granule] of the type's size, and a u16 sector count per block - as
/// long as validate checks they are. Everything past the pointers is found
/// through csum_type: for one this version doesn't know - a newer version's
/// stripe, which validate passes on purpose - csums() and blockcounts() are
/// None, and the stripe can't be updated.
impl c::Stripe<Validated> {
    pub fn block_ptrs(&self) -> &[c::bch_extent_ptr] {
        cast_plain_slice(&self.ptrs[..self.ptrs_bytes()]).expect("a stripe's pointers, aligned")
    }

    pub fn block_ptrs_mut(&mut self) -> &mut [c::bch_extent_ptr] {
        let n = self.ptrs_bytes();
        cast_plain_slice_mut(&mut self.ptrs[..n]).expect("a stripe's pointers, aligned")
    }

    /// The checksums, a 2D array of [block][granule] - None for a checksum
    /// type this version doesn't know.
    pub fn csums(&self) -> Option<StripeCsums<&[u8]>> {
        let at = self.csums_at()?;
        Some(StripeCsums { bytes: &self.ptrs[at], csum_bytes: self.csum_bytes()? as usize,
                           per_device: self.csums_per_device() as usize })
    }

    pub fn csums_mut(&mut self) -> Option<StripeCsums<&mut [u8]>> {
        let (at, csum_bytes, per_device) = (self.csums_at()?, self.csum_bytes()? as usize,
                                            self.csums_per_device() as usize);
        Some(StripeCsums { bytes: &mut self.ptrs[at], csum_bytes, per_device })
    }

    pub fn blockcounts(&self) -> Option<&[Le16]> {
        let at = self.blockcounts_at()?;
        Some(cast_plain_slice(&self.ptrs[at]).expect("a stripe's block counts, aligned"))
    }

    pub fn blockcounts_mut(&mut self) -> Option<&mut [Le16]> {
        let at = self.blockcounts_at()?;
        Some(cast_plain_slice_mut(&mut self.ptrs[at]).expect("a stripe's block counts, aligned"))
    }

    /// Its time in the stripe lru: 0, not in it, if it has no room for new
    /// data; STRIPE_LRU_POS_EMPTY if it has no data, for the stripe_delete
    /// worker; else earlier the more it can take - empty data blocks, and
    /// blocks it could be widened by - to be reused first. Not in it either
    /// with a checksum type this version doesn't know: it can't be reused or
    /// deleted, which would update it.
    ///
    /// Written by the stripe trigger, checked by check_lrus.
    pub fn lru_pos(&self) -> u64 {
        let Some(counts) = self.blockcounts() else { return 0 };
        let nr_data = self.nr_data();
        let blocks_empty = counts.iter().take(nr_data as usize).filter(|n| n.get() == 0).count() as u32;

        if blocks_empty == nr_data {
            return STRIPE_LRU_POS_EMPTY;
        }

        match blocks_empty + u32::from(self.can_widen()) {
            0        => 0,
            reusable => c::LRU_TIME_MAX - u64::from(reusable),
        }
    }
}

/// A stripe's checksums: a 2D array of [block][granule], each csum_bytes
/// wide - the low bytes of a bch_csum.
pub struct StripeCsums<B> {
    bytes:      B,
    csum_bytes: usize,
    per_device: usize,
}

impl<B> StripeCsums<B> {
    fn at(&self, block: u32, granule: u32) -> Range<usize> {
        debug_assert!((granule as usize) < self.per_device);
        let start = (block as usize * self.per_device + granule as usize) * self.csum_bytes;
        start..start + self.csum_bytes
    }
}

impl<B: AsRef<[u8]>> StripeCsums<B> {
    /// stripe_csum_get().
    pub fn get(&self, block: u32, granule: u32) -> c::bch_csum {
        let mut csum = c::bch_csum::default();
        plain_bytes_mut(&mut csum)[..self.csum_bytes].copy_from_slice(&self.bytes.as_ref()[self.at(block, granule)]);
        csum
    }
}

impl<B: AsMut<[u8]>> StripeCsums<B> {
    /// stripe_csum_set().
    pub fn set(&mut self, block: u32, granule: u32, csum: c::bch_csum) {
        let at = self.at(block, granule);
        self.bytes.as_mut()[at].copy_from_slice(&plain_bytes(&csum)[..self.csum_bytes]);
    }
}

c_const! {
    pub const STRIPE_LRU_POS_EMPTY: u64 = 1;
}

/// Whether @p is the data block its stripe_ptr names in @s:
/// bch2_ptr_matches_stripe(). A block past the value matches nothing.
pub fn ptr_matches_stripe(s: BkeySC<'_, c::Stripe<Validated>>, p: &c::extent_ptr_decoded) -> bool {
    assert!(p.has_ec, "matching a pointer without a stripe against one");

    let v = s.v();
    let block = u32::from(p.ec.block());
    if block >= v.nr_data() {
        return false;
    }
    let Some(sp) = v.block_ptrs().get(block as usize) else { return false };

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
pub(crate) fn ptr_in_block(stripe_ptr: &c::bch_extent_ptr, data_ptr: &c::bch_extent_ptr, sectors: u64) -> bool {
    let invalid = c::BCH_SB_MEMBER_INVALID as u8;
    (data_ptr.dev() == stripe_ptr.dev() || data_ptr.dev() == invalid || stripe_ptr.dev() == invalid) &&
        data_ptr.generation() == stripe_ptr.generation() &&
        data_ptr.offset() >= stripe_ptr.offset() &&
        data_ptr.offset() < stripe_ptr.offset() + sectors
}


/// How many data blocks @nr_devs devices could give a stripe with
/// @nr_redundant parity blocks - the pool's widening target - capped at
/// BCH_BKEY_PTRS_MAX, and at @max_data_blocks if that's set (the
/// ec_max_data_blocks option): stripe_widen_target_nr_data().
fn stripe_widen_target_nr_data(nr_devs: u32, nr_redundant: u32, max_data_blocks: u32) -> u32 {
    if nr_devs <= nr_redundant {
        return 0;
    }
    // unsigned, as C's
    let t = nr_devs.min(c::BCH_BKEY_PTRS_MAX).wrapping_sub(nr_redundant);
    if max_data_blocks != 0 { t.min(max_data_blocks) } else { t }
}

/// What a stripe's can_widen holds: target - actual data blocks, clamped to
/// its three bits.
fn stripe_widen_value(target_nr_data: u32, actual_nr_data: u32) -> u8 {
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

    // nr_redundant past nr_blocks is damage, printed as is
    write!(out, "algo {} sectors {} blocks {}:{} csum {} gran ",
           s.algorithm(), s.sectors.get(), s.nr_data(), s.nr_redundant, c::bch_csum_type(s.csum_type.into()));
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

    let tail = &val[STRIPE_PTRS..];
    let ptrs = &tail[..s.ptrs_bytes().min(tail.len()) / 8 * 8];
    let ptrs: &[c::bch_extent_ptr] = cast_plain_slice(ptrs).expect("a stripe's pointers, aligned");
    let counts = if s.csum_granularity_bits < 31 { s.blockcounts_at() } else { None };

    for (i, ptr) in ptrs.iter().enumerate() {
        out.newline();
        extent_ptr_to_text(&mut out, Some(fs), ptr, &rcu);

        if let Some(n) = counts.as_ref().and_then(|at| tail.get(at.start + 2 * i..at.start + 2 * i + 2)) {
            write!(out, "#{}", u16::from_le_bytes([n[0], n[1]]));
        }
    }
}


/// A stripe, as the trigger sees one going in or out.
pub(crate) type StripeK<'a> = BkeySC<'a, c::Stripe<Validated>>;


/// A block of a stripe: the stripe, and one of its blocks by index - one it
/// has, as only the stripe makes them. The index travels with its stripe, so
/// it can't be used with another: what's wanted of a block - its pointer, its
/// number, its sector count - is asked of the Block.
#[derive(Clone, Copy)]
pub(crate) struct Block<'k> {
    s:   StripeK<'k>,
    idx: u8,
}

impl<'k> StripeK<'k> {
    /// Its data blocks, as far as it has pointers: nr_redundant past
    /// nr_blocks, damage validate passes, would wrap nr_data().
    pub(crate) fn data_blocks(self) -> impl Iterator<Item = Block<'k>> {
        let v = self.v();
        let nr = (v.nr_data() as usize).min(v.block_ptrs().len());
        (0..nr as u8).map(move |idx| Block { s: self, idx })
    }

    /// Data block @i, if it has one.
    pub(crate) fn data_block(self, i: usize) -> Option<Block<'k>> {
        self.data_blocks().nth(i)
    }
}

impl<'k> Block<'k> {
    pub(crate) fn stripe(self) -> StripeK<'k> {
        self.s
    }

    pub(crate) fn idx(self) -> u8 {
        self.idx
    }

    pub(crate) fn ptr(self) -> c::bch_extent_ptr {
        self.s.v().block_ptrs()[usize::from(self.idx)]
    }

    /// Its sector count. Of a stripe of a checksum type this version knows:
    /// one the trigger's csum_type_check() passed, or one in the stripe lru -
    /// copygc's - which lru_pos() keeps the others out of.
    pub(crate) fn sectors(self) -> u16 {
        let counts = self.s.v().blockcounts().expect("a stripe of a checksum type this version knows");
        counts[usize::from(self.idx)].get()
    }
}

/// The blocks of @old that block map @map says @new carries - data block j
/// of @new is @old's map[j], for the first map.len() - checked against the
/// two: each one of @old's data blocks, none twice, and no more than @new has
/// data blocks. A stripe update's map is checked here, as a crashed one's
/// comes from disk.
pub(crate) fn carried_blocks<'k>(map: &[u8], old: StripeK<'k>, new: StripeK<'_>)
    -> Result<ArrayVec<Block<'k>, MAX_BLOCKS>, &'static str>
{
    if map.len() > new.data_blocks().count() {
        return Err("more blocks mapped than the new stripe has data blocks");
    }

    let mut carried: ArrayVec<Block<'k>, MAX_BLOCKS> = ArrayVec::new();
    for &i in map {
        let b = old.data_block(i.into()).ok_or("a block mapped that isn't one of the old stripe's data blocks")?;
        if carried.iter().any(|c| c.idx == i) {
            return Err("an old block mapped twice");
        }
        carried.push(b);
    }
    Ok(carried)
}


// ── The widen cache ──────────────────────────────────────────────────────

/// How many RW members each (disk_label, sectors) has - the widening target
/// a stripe there has - looked up once each: C's widen_cache. Sorted by
/// (disk_label, sectors).
#[derive(Default)]
pub struct WidenCache(KVVec<((u8, u16), u32)>);

impl WidenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// @disk_label's RW members, for a stripe of @sectors.
    pub fn lookup(&mut self, fs: &Fs, disk_label: u8, sectors: u16) -> Result<u32, BchError> {
        let key = (disk_label, sectors);
        match self.0.binary_search_by_key(&key, |e| e.0) {
            Ok(i) => Ok(self.0[i].1),
            Err(i) => {
                let nr = fs.ec_rw_member_devs_nr(disk_label, sectors);
                self.0.push((key, nr), GFP_KERNEL)?;
                self.0[i..].rotate_right(1);
                Ok(nr)
            }
        }
    }

    /// The can_widen stripe @s should have now: how many more data blocks
    /// its disk label's RW members would let it have.
    pub fn can_widen(&mut self, fs: &Fs, s: &c::Stripe) -> Result<u8, BchError> {
        let nr_devs = self.lookup(fs, s.disk_label, s.sectors.get())?;
        Ok(s.want_can_widen(nr_devs, fs.opts().ec_max_data_blocks.into()))
    }
}


// ── A stripe's key ───────────────────────────────────────────────────────

/// The stripe's shape, copied out of its header.
#[derive(Clone, Copy)]
pub(crate) struct Shape {
    pub(crate) nr_blocks:             u32,
    pub(crate) nr_redundant:          u32,
    /// unsigned, as C's - nr_redundant past nr_blocks is damage
    pub(crate) nr_data:               u32,
    pub(crate) sectors:               u32,
    pub(crate) csum_type:             u8,
    pub(crate) csum_granularity_bits: u8,
    pub(crate) csums_per_device:      u32,
}

pub(crate) fn shape(s: BkeySC<'_, c::Stripe<Validated>>) -> Shape {
    let v = s.v();
    Shape {
        nr_blocks:             v.nr_blocks.into(),
        nr_redundant:          v.nr_redundant.into(),
        nr_data:               v.nr_data(),
        sectors:               v.sectors.get().into(),
        csum_type:             v.csum_type,
        csum_granularity_bits: v.csum_granularity_bits,
        csums_per_device:      v.csums_per_device(),
    }
}

/// A stripe's key with room for all of its value - as much as any key holds,
/// its u64s being a u8: the key of a stripe being made, edited as its blocks
/// are allocated. Zeroed, it's a deleted key, no stripe.
#[repr(C)]
#[derive(Clone)]
pub struct StripeKey([u64; c::BKEY_U64s_MAX as usize]);

// SAFETY: u64s
unsafe impl ::pin_init::Zeroable for StripeKey {}

impl StripeKey {
    fn room(&self) -> &[u8] {
        plain_bytes(&self.0)
    }

    fn room_mut(&mut self) -> &mut [u8] {
        plain_bytes_mut(&mut self.0)
    }

    /// Its stripe: the key, as long as its u64s say.
    pub(crate) fn stripe(&self) -> BkeySC<'_, c::Stripe<Validated>> {
        let room = self.room();
        let len = cast_plain::<c::bkey>(&room[..size_of::<c::bkey>()]).expect("a key's header").bytes();
        assert!(len <= room.len(), "a stripe key of {len} bytes, in {} of room", room.len());

        // SAFETY: a key, all its u64s in @room - which we have borrowed
        let k: &BkeyI = unsafe { &*BkeyI::from_raw(room.as_ptr().cast()) };
        BkeySC::from(k).arm::<c::Stripe>().expect("a StripeKey's key is a stripe")
    }

    /// Its slot: the stripe's index.
    pub(crate) fn idx(&self) -> u64 {
        self.stripe().k.p.offset
    }

    /// Make it a copy of @k, in place: it's too big to pass by value.
    pub(crate) fn copy_from(&mut self, k: &StripeKey) {
        self.0 = k.0;
    }

    /// Make it @k: bkey_reassemble().
    pub fn set(&mut self, k: BkeySC<'_, c::Stripe<Validated>>) {
        let (hdr, val) = (size_of::<c::bkey>(), k.val_bytes());
        let room = self.room_mut();
        assert!(hdr + val.len() <= room.len(), "a stripe of {} bytes, too big for a key", val.len());

        room[..hdr].copy_from_slice(plain_bytes(k.k));
        room[hdr..hdr + val.len()].copy_from_slice(val);
    }

    /// Make it a new stripe, @v its header: bkey_stripe_init(), and the value
    /// as long as @v says. Past the header it's as it was - zeroed, for a new
    /// stripe.
    pub(crate) fn init(&mut self, v: &c::Stripe) {
        let mut k = c::bkey::new();
        k.type_ = c::bch_bkey_type::KEY_TYPE_stripe.0 as u8;
        k.set_val_bytes(v.val_u64s() as usize * size_of::<u64>());

        let (hdr, room) = (size_of::<c::bkey>(), self.room_mut());
        assert!(k.bytes() <= room.len(), "a new stripe of {} bytes, too big for a key", k.bytes());
        room[..hdr].copy_from_slice(plain_bytes(&k));
        room[hdr..hdr + size_of::<c::Stripe>()].copy_from_slice(plain_bytes(v));
    }

    /// Its header, to edit: its position, say.
    pub(crate) fn k_mut(&mut self) -> &mut c::bkey {
        cast_plain_mut(&mut self.room_mut()[..size_of::<c::bkey>()]).expect("a key's header")
    }

    /// Its stripe's value, to edit.
    pub(crate) fn stripe_mut(&mut self) -> &mut c::Stripe<Validated> {
        let (hdr, len) = (size_of::<c::bkey>(), self.stripe().val_bytes().len());
        cast_mut(&mut self.room_mut()[hdr..hdr + len]).expect("a stripe's value")
    }
}
