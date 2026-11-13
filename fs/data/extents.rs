//! Extents: keys with a size, a range of their btree's offsets - what's
//! cut, merged and counted as file data. Their pointers, where they have
//! them, are data/ptrs.rs's.

#![allow(non_camel_case_types)]

use crate::bkey_fsck_err_on;
use crate::btree::bkey::{bkey_ge, bkey_gt, bkey_le, bkey_lt, types_with, AllTypes, BkeyFamily, ByFlag, FamilyTypes, FamilyVal, InFamily, KeyState, BkeyS, BkeySC, BkeyV, BkeyVal,
                         BkeyVMut};
use crate::btree::bkey::BkeyOps;
use crate::btree::trigger::Trigger;
use crate::btree::iter::NoRoom;
use crate::c;
use crate::data::inline_data::{InlineDataMut, InlineDataRef};
use crate::data::keylist::KeyRoom;
use crate::data::ptrs::{crc_is_compressed, crc_unpack, BkeyVPtrs, ExtentEntry, ExtentEntryMut, IoFailures, Ptrs, PtrsMut};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::vstructs::Validated;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{MaybeZeroable, bch_enum, TypeInfo};
use crate::types::c_default;
use crate::format::{Bpos, Btree, BkeyType, BkeyTypeFlags, BkeyTypeFlags_bits};

#[repr(C, align(8))]
#[derive(Clone, Copy, MaybeZeroable, TypeInfo)]
pub union bch_extent_crc {
    pub type_: u8,
    pub crc32: c::bch_extent_crc32,
    pub crc64: c::bch_extent_crc64,
    pub crc128: c::bch_extent_crc128,
}
c_default!(bch_extent_crc);

/* bkey_ptrs: generically over any key type that has ptrs */
#[repr(C)]
#[derive(Clone, Copy, MaybeZeroable)]
pub struct bkey_ptrs_c {
    pub start: *const c::bch_extent_entry,
    pub end: *const c::bch_extent_entry,
}
c_default!(bkey_ptrs_c);

#[repr(C)]
#[derive(Clone, Copy, MaybeZeroable)]
pub struct bkey_ptrs {
    pub start: *mut c::bch_extent_entry,
    pub end: *mut c::bch_extent_entry,
}
c_default!(bkey_ptrs);

/*
 * Everything one walk of a key's pointer list can say about its replication.
 *
 * Gathered together because the walk is the expensive part - and for the exact
 * version, a stripe read per erasure coded pointer - while callers routinely
 * want several of these at once. bch2_sum_sector_overwrites() asks four
 * separate single-value helpers for them, on the same two keys, three of the
 * calls inside a loop.
 *
 * The durability counts are weighted by each device's mi.durability and skip
 * BCH_SB_MEMBER_INVALID placeholders - those are added by
 * bch2_bkey_set_needs_reconcile() on a degraded write, to stand for a replica
 * that isn't there. The raw counts below are unweighted.
 *
 * u8 except the sector count: BCH_MEMBER_DURABILITY is a two bit field and
 * BCH_REPLICAS_MAX is 4, so none of these can come near 255.
 *
 * Not handled here: KEY_TYPE_reservation, which several of the older
 * single-value helpers report as v->nr_replicas. Callers that need that still
 * special-case it themselves.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct bkey_durability {
    pub online: u8,
    pub total: u8,
    pub acct: u8,
    pub min_durability: u8,

    pub nr_ptrs: u8, /* real device pointers */
    pub nr_overwritable: u8, /* uncompressed - an overwrite reclaims these */
    pub sectors_compressed: c_uint,

    /*
     * Copies this key occupies, for disk space accounting.
     *
     * Deliberately not weighted by mi.durability, unlike the counts above:
     * durability is OPT_RUNTIME, and accounting is persistent, so a
     * durability change would retroactively invalidate space already
     * accounted for. This has to be a function of what is physically on
     * disk.
     *
     * Differs from nr_ptrs only for a reservation, which occupies the space
     * it reserved while having no pointers at all.
     */
    pub nr_replicas: u8,

    /*
     * Copies for the purpose of "does this write increase replication" -
     * erasure coding counts, because a stripe genuinely provides it.
     *
     * Distinct from nr_replicas on purpose: parity is accounted separately
     * as BCH_DATA_parity at the stripe, so counting redundancy in a
     * per-extent space figure would charge the same parity to every extent
     * sharing the stripe. Space and replication are different questions.
     */
    pub replicas: u8,
}

#[repr(C)]
/// Equal as bch2_crc_unpacked_cmp() says: every field.
#[derive(Clone, Copy, Default, PartialEq, Eq, MaybeZeroable, TypeInfo)]
pub struct bch_extent_crc_unpacked {
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub live_size: u32,

    pub csum_type: u8,
    pub compression_type: u8,

    pub offset: u16,

    pub nonce: u16,

    pub csum: c::bch_csum,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct extent_ptr_decoded {
    pub has_ec: bool,
    pub do_ec_reconstruct: bool,
    pub crc_retry_nr: u8,
    pub crc: c::bch_extent_crc_unpacked,
    pub ptr: c::bch_extent_ptr,
    pub ec: c::bch_extent_stripe_ptr,
}

/// What failed reading or writing a key's pointers, for C: its size, not its
/// layout - C asks it what it holds.
#[repr(C)]
#[derive(Default, MaybeZeroable, TypeInfo)]
pub struct bch_io_failures {
    pub(crate) failures: IoFailures<{ c::BCH_BKEY_PTRS_MAX as usize }>,
}

#[bch_enum(closed)]
#[prefix(__BCH_READ_)]
#[mask(ReadFlags: u32, prefix = BCH_READ_)]
#[repr(u32)]
pub enum ReadFlagBit {
    retry_if_stale,
    may_promote,
    user_mapped,
    soft_require_read_device,
    hard_require_read_device,
    must_bounce,
    no_poison_check,
}

/*
 * BkeyTypeFlags::extent flags the key types an extents btree may hold -
 * checked against Btree's, which is what says: deleted, which every btree
 * may, and what each extents btree #[holds].
 */
const _: () = {
    let (mut extents_btrees_key_types, mut id) = (0, 0);
    while id < Btree::NR as u32 {
        if crate::BTREE_IS_EXTENTS_MASK & (1 << id) != 0 {
            extents_btrees_key_types |= Btree(id).key_types();
        }
        id += 1;
    }
    assert!(types_with(BkeyTypeFlags::extent) == extents_btrees_key_types,
            "BkeyTypeFlags::extent isn't on exactly the key types extents btrees hold");
};

/// Any key an extents btree may hold, its size nonzero - a range of its btree,
/// start_pos() to k.p: a key armed to it is a BkeySC<'_, Extents>,
/// BkeyS<'_, Extents>, &BkeyI<Extents>. A key is one by its size as well as its
/// type: a deletion slot iteration makes is one or not by the iterator's mode.
pub type Extents = FamilyVal<ExtentsTypes>;

pub enum ExtentsTypes {}

impl FamilyTypes for ExtentsTypes {
    fn contains(k: &c::bkey) -> bool {
        ByFlag::<{ BkeyTypeFlags_bits::extent }>::contains(k) && k.size != 0
    }
}

// SAFETY: every key is one of AllTypes'
unsafe impl<S: KeyState> InFamily<AllTypes> for FamilyVal<ExtentsTypes, S> {}

/// A key counted in its inode's i_sectors, BkeyTypeFlags::allocation.
pub type Allocation = FamilyVal<ByFlag<{ BkeyTypeFlags_bits::allocation }>>;

/// Data a file reads, BkeyTypeFlags::data: pointers to it, the data inline, or a
/// reflink_p to it.
pub type Data = FamilyVal<ByFlag<{ BkeyTypeFlags_bits::data }>>;

/// A file's data in the extents btree, not through a reflink_p,
/// BkeyTypeFlags::user_data.
pub type UserData = FamilyVal<ByFlag<{ BkeyTypeFlags_bits::user_data }>>;

impl c::bkey {
    pub fn extent_is_allocation(&self) -> bool {
        <Allocation as BkeyFamily>::contains(self)
    }

    pub fn extent_is_data(&self) -> bool {
        <Data as BkeyFamily>::contains(self)
    }

    pub fn is_user_data(&self) -> bool {
        <UserData as BkeyFamily>::contains(self)
    }

    /// Whether its value is pointers to its data, and not to another key's:
    /// a PtrsMut key - every type with pointers but a stripe.
    pub fn extent_is_direct_data(&self) -> bool {
        <PtrsMut as BkeyFamily>::contains(self)
    }
}

/*
 * A cut that leaves nothing - which makes the key a size 0 deletion - gives
 * back None, not an empty extent: what's left isn't to be inserted, and the
 * caller branches on it. One that leaves anything leaves its type, so it's
 * still an extent.
 */

/// What a key does as an extent, in whichever form it's held: any key's
/// unwritten and reservation tests, and an extent's cuts - one armed to Extents.
pub trait BkeyVExtents: BkeyV {
    /// Whether it has space allocated and not yet written: an unwritten
    /// pointer.
    fn extent_is_unwritten(&self, fs: &Fs) -> bool
    where Self::V: BkeyVal<State = Validated>
    {
        self.as_sc().arm::<Ptrs>().is_some_and(|k| k.ptrs(fs).any(|p| p.unwritten() != 0))
    }

    /// Whether it reserves space rather than holding data - a reservation, or
    /// an extent with unwritten pointers.
    fn extent_is_reservation(&self, fs: &Fs) -> bool
    where Self::V: BkeyVal<State = Validated>
    {
        self.k().key_type() == BkeyType::reservation || self.extent_is_unwritten(fs)
    }

    /// Cut to start at @where_: bch2_cut_front_s().
    fn cut_front(mut self, fs: &Fs, where_: Bpos) -> Option<Self>
    where Self: BkeyVMut<V = Extents>
    {
        self.cut_front_in_place(fs, where_);
        (self.k().size != 0).then_some(self)
    }

    /// Cut to end at @where_: bch2_cut_back_s().
    fn cut_back(mut self, where_: Bpos) -> Option<Self>
    where Self: BkeyVMut<V = Extents>
    {
        self.cut_back_in_place(where_);
        (self.k().size != 0).then_some(self)
    }

    /// Cut to start at @where_, if it starts before it: what's cut off is
    /// skipped in its value too - a pointer's offset moved on, or a crc's
    /// into its data, a reflink_p's idx, inline data dropped - and a cut that
    /// leaves nothing makes it a deletion. The u64s its value gives up are
    /// zeroed. bch2_cut_front_s().
    fn cut_front_in_place(&mut self, fs: &Fs, where_: Bpos)
    where Self: BkeyVMut<V = Extents>
    {

        let mut k = self.as_s().untyped();
        if bkey_le(where_, k.k.start_pos()) {
            return;
        }
        debug_assert!(!bkey_gt(where_, k.k.p), "cutting the front of an extent past its end");

        // as C's: a u64 into a u32 size, wrapping
        let sub = where_.offset.wrapping_sub(k.k.start_offset());
        k.k.size = k.k.size.wrapping_sub(sub as u32);

        let mut new_val_u64s = k.k.val_u64s();
        if k.k.size == 0 {
            k.k.set_key_type(BkeyType::deleted);
            new_val_u64s = 0;
        }

        match k.k.key_type() {
            BkeyType::extent | BkeyType::reflink_v => {
                let mut p = k.as_s().arm::<Ptrs>().expect("an extent has pointers");
                // a pointer after a crc points to the crc's data, whose
                // offset moves instead
                let mut seen_crc = false;
                let pk = *p.k();

                for e in p.extent_entries_mut(fs) {
                    match e {
                        ExtentEntryMut::ptr(ptr) => if !seen_crc {
                            ptr.update_offset(|o| o + sub);
                        },
                        mut e => if let Some(mut crc) = crc_unpack(&pk, e.as_entry()) {
                            let old_offset = crc.offset;
                            crc.offset = (u64::from(crc.offset) + sub) as u16;
                            // C's bitfield would truncate: past its entry, the offset was
                            // past its data - a cut past the key's end, or a key whose
                            // crc offset + size was past its uncompressed size
                            if e.set_crc(&crc).is_none() {
                                panic!("cutting an extent's front at {where_}, by {sub}, leaving {pk}: crc offset \
                                        {old_offset} + {sub} is past what its entry holds (uncompressed size {}, \
                                        compressed {})",
                                       crc.uncompressed_size, crc.compressed_size);
                            }
                            seen_crc = true;
                        },
                    }
                }
            }
            BkeyType::reflink_p => {
                let mut p = k.as_s().arm::<c::ReflinkP>().expect("a reflink_p");
                let v = p.v_mut();
                v.update_idx(|i| i + sub);
            }
            _ if k.k.extent_is_inline_data() => {
                let d = InlineDataMut::of(k.as_s()).expect("inline data");
                if let Some(h) = d.csum {
                    h.offset.set(h.offset.get() + sub as u32);
                } else {
                    let sub = (sub << 9).min(d.data.len() as u64) as usize;

                    d.data.copy_within(sub.., 0);
                    new_val_u64s -= sub / size_of::<u64>();
                }
            }
            _ => {}
        }

        shrink_val(k, new_val_u64s);
    }

    /// Cut to end at @where_, if it ends after it - inline data past it
    /// dropped - and a cut that leaves nothing makes it a deletion. The u64s
    /// its value gives up are zeroed. bch2_cut_back_s().
    fn cut_back_in_place(&mut self, where_: Bpos)
    where Self: BkeyVMut<V = Extents>
    {

        let k = self.as_s().untyped();
        if bkey_ge(where_, k.k.p) {
            return;
        }
        debug_assert!(!bkey_lt(where_, k.k.start_pos()), "cutting the back of an extent before its start");

        let len = where_.offset.wrapping_sub(k.k.start_offset());
        k.k.p.offset = where_.offset;
        k.k.size = len as u32;

        let mut new_val_u64s = k.k.val_u64s();
        if len == 0 {
            k.k.set_key_type(BkeyType::deleted);
            new_val_u64s = 0;
        }

        if let Some(d) = InlineDataRef::of(k.as_sc()).filter(|d| d.csum.is_none()) {
            let off = k.k.val_bytes() - d.data.len();
            new_val_u64s = (off + d.data.len().min((k.k.size as usize) << 9)) / size_of::<u64>();
        }

        shrink_val(k, new_val_u64s);
    }
}

impl<K: BkeyV> BkeyVExtents for K {}

/// @k's value made @val_u64s long, the u64s past it zeroed: a cut's end. It
/// never grows.
fn shrink_val(mut k: BkeyS<'_>, val_u64s: usize) {
    let val = k.val_bytes_mut();
    let bytes = val_u64s * size_of::<u64>();
    assert!(bytes <= val.len(), "a cut growing its key's value, {} to {bytes} bytes", val.len());

    val[bytes..].fill(0);
    k.k.set_val_bytes(bytes);
}

/// The change in @k's u64s, never above 0.
pub fn bch2_cut_front_s(c: &Fs, where_: Bpos, k: BkeyS<'_>) -> c_int {
    let u64s = c_int::from(k.k.u64s);
    let mut k = k.arm::<Extents>().expect("cutting a key an extents btree can't hold");
    k.cut_front_in_place(c, where_);
    c_int::from(k.k().u64s) - u64s
}

/// The change in @k's u64s, never above 0.
pub fn bch2_cut_back_s(where_: Bpos, k: BkeyS<'_>) -> c_int {
    let u64s = c_int::from(k.k.u64s);
    let mut k = k.arm::<Extents>().expect("cutting a key an extents btree can't hold");
    k.cut_back_in_place(where_);
    c_int::from(k.k().u64s) - u64s
}

// ── appending pointers ───────────────────────────────────────────────────

/// @ptr after @k's value - its last entry - and @k a u64 longer: @k a key
/// with pointers - btree_ptr, btree_ptr_v2, extent - and @ptr to a device it
/// has none on.
pub fn bkey_append_ptr(k: &mut KeyRoom<'_>, ptr: c::bch_extent_ptr) -> Result<(), NoRoom> {
    let t = k.k().key_type();
    assert!(matches!(t, BkeyType::btree_ptr | BkeyType::btree_ptr_v2 | BkeyType::extent),
            "a pointer appended to a key of type {}", t.0);
    k.append_val(&[ptr.into_bits().0])
}

// ── merging ──────────────────────────────────────────────────────────────

/// Which crc covers two adjacent ranges' data merged.
enum CrcMerge {
    /// Left's: its data runs on past both ranges.
    Left,
    /// Right's: its data starts before left's range.
    Right,
    /// Neither: their checksums merged, if they can be.
    Both,
}

fn crc_merge(l: &c::bch_extent_crc_unpacked, r: &c::bch_extent_crc_unpacked) -> CrcMerge {
    if u32::from(l.offset) + l.live_size + r.live_size <= l.uncompressed_size {
        CrcMerge::Left
    } else if l.live_size <= u32::from(r.offset) {
        CrcMerge::Right
    } else {
        CrcMerge::Both
    }
}

/// Whether @r continues @l on disk, so the two can be one pointer of the
/// merged extent.
fn ptrs_mergeable(fs: &Fs, rcu: &ReadGuard, l: &c::extent_ptr_decoded, r: &c::extent_ptr_decoded) -> bool {
    let (lc, rc) = (&l.crc, &r.crc);

    if l.ptr.offset() + u64::from(lc.offset) + u64::from(lc.live_size) != r.ptr.offset() + u64::from(rc.offset) ||
        l.ptr.dev() != r.ptr.dev() ||
        l.ptr.generation() != r.ptr.generation() ||
        l.ptr.unwritten() != r.ptr.unwritten() ||
        l.has_ec != r.has_ec {
        return false;
    }

    // Extents may not straddle buckets:
    match fs.dev_rcu(l.ptr.dev().into(), rcu) {
        Some(ca) if ca.ptr_bucket(&l.ptr) == ca.ptr_bucket(&r.ptr) => {}
        _ => return false,
    }

    if l.has_ec &&
        (l.ec.block() != r.ec.block() ||
         l.ec.redundancy() != r.ec.redundancy() ||
         l.ec.idx() != r.ec.idx()) {
        return false;
    }

    if lc.compression_type != rc.compression_type || lc.nonce != rc.nonce {
        return false;
    }

    /*
     * Compressed extents may only merge if they're pieces of the same
     * compressed blob. The address continuity check above doesn't establish
     * that: for compressed extents it adds uncompressed sectors (crc.offset,
     * live_size) to a device address (ptr.offset), so pieces of two different
     * blobs can satisfy it by coincidence - and then merging remaps one range
     * onto the other blob's data:
     */
    if crc_is_compressed(lc) &&
        (l.ptr.offset() != r.ptr.offset() ||
         lc.uncompressed_size != rc.uncompressed_size ||
         lc.compressed_size != rc.compressed_size ||
         lc.csum.lo != rc.csum.lo || lc.csum.hi != rc.csum.hi) {
        return false;
    }

    match crc_merge(lc, rc) {
        CrcMerge::Left | CrcMerge::Right => true,
        // the checksums merge, of adjacent ranges each checksummed whole,
        // into one as long as a checksummed extent may be
        CrcMerge::Both =>
            lc.csum_type == rc.csum_type &&
            lc.nonce == rc.nonce &&
            !crc_is_compressed(lc) &&
            c::CsumType(lc.csum_type.into()).mergeable() &&
            u32::from(lc.offset) + lc.live_size == lc.compressed_size &&
            rc.offset == 0 &&
            (lc.csum_type == 0 ||
             lc.uncompressed_size + rc.uncompressed_size <= fs.opts().encoded_extent_max.get() >> 9),
    }
}

/// The largest data a crc entry of @e's type can describe, if @e is one:
/// bch2_crc_field_size_max[].
fn crc_size_max(e: ExtentEntry<'_>) -> Option<u32> {
    Some(match e {
        ExtentEntry::crc32(_)  => c::CRC32_SIZE_MAX,
        ExtentEntry::crc64(_)  => c::CRC64_SIZE_MAX,
        ExtentEntry::crc128(_) => c::CRC128_SIZE_MAX,
        _ => return None,
    })
}

/// Whether @l and @r, @r starting where @l ends, can be one extent - their
/// entries pairing up by type, each pointer pair continuing on disk - and if
/// so, @l made it: bch2_extent_merge(). The caller checked the merged size
/// fits a key.
pub fn extent_merge(fs: &Fs, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
    let mut l = l.arm::<Ptrs>().expect("->key_merge of a key type with pointers");
    let r = r.arm::<Ptrs>().expect("->key_merge of a key type with pointers");

    if !l.extent_entries(fs).map(|e| e.ty()).eq(r.extent_entries(fs).map(|e| e.ty())) {
        return false;
    }

    {
        let rcu = read_lock();
        if !l.ptrs_decoded(fs).zip(r.ptrs_decoded(fs)).all(|(lp, rp)| ptrs_mergeable(fs, &rcu, &lp.p, &rp.p)) {
            return false;
        }
    }

    // Each crc entry pair's data, summed, must fit their type - whichever crc
    // the pair takes:
    let (lk, rk) = (*l.k(), *r.k());
    let size = |k: &c::bkey, e: ExtentEntry<'_>| crc_unpack(k, e).expect("a crc entry pair: their types matched").uncompressed_size;
    for (el, er) in l.extent_entries(fs).zip(r.extent_entries(fs)) {
        if crc_size_max(el).is_some_and(|max| size(&lk, el) + size(&rk, er) > max) {
            return false;
        }
    }

    let mut use_right_ptr = false;
    for (el, er) in l.extent_entries_mut(fs).zip(r.extent_entries(fs)) {
        let mut el = match (el, er) {
            (ExtentEntryMut::ptr(lp), ExtentEntry::ptr(rp)) => {
                if use_right_ptr {
                    *lp = *rp;
                }
                continue;
            }
            (el, _) => el,
        };
        let Some(mut cl) = crc_unpack(&lk, el.as_entry()) else { continue };
        let mut cr = crc_unpack(&rk, er).expect("a crc entry pair: their types matched");

        use_right_ptr = false;
        match crc_merge(&cl, &cr) {
            CrcMerge::Left => {}
            CrcMerge::Right => {
                // live_size <= offset, by crc_merge(): it fits
                cr.offset -= cl.live_size as u16;
                el.set_crc(&cr).expect("right's crc, its offset less, fits the entry right's was in");
                use_right_ptr = true;
            }
            CrcMerge::Both => {
                // The csum type is mergeable, as merge() panics if it isn't:
                // ptrs_mergeable() checked it, with the pointer after this
                // entry - validate has one after every crc entry
                cl.csum = c::CsumType(cl.csum_type.into())
                    .merge(cl.csum, cr.csum, (cr.uncompressed_size << 9) as usize);
                cl.uncompressed_size += cr.uncompressed_size;
                cl.compressed_size += cr.compressed_size;
                el.set_crc(&cl).expect("a merged crc fits: its size was checked against the entry's");
            }
        }
    }

    let size = l.k().size + r.k.size;
    l.as_s().k.resize(size);
    true
}

// ── reservations ─────────────────────────────────────────────────────────

impl BkeyOps for c::Reservation {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        let r = v.k.arm::<c::Reservation>().expect("a reservation").v();

        bkey_fsck_err_on!(v, r.nr_replicas == 0 || u32::from(r.nr_replicas) > c::BCH_REPLICAS_MAX,
                          id::reservation_key_nr_replicas_invalid,
                          "invalid nr_replicas ({})", r.nr_replicas)
    }

    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        let r = k.arm::<c::Reservation>().expect("a reservation").v();

        write!(out, "generation {} replicas {}", r.generation.get(), r.nr_replicas);
    }

    /// Adjacent reservations of the same generation and replicas are one.
    fn merge(_fs: &Fs, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
        let lv = l.as_sc().arm::<c::Reservation>().expect("a reservation").v();
        let rv = r.arm::<c::Reservation>().expect("a reservation").v();

        if lv.generation.get() != rv.generation.get() || lv.nr_replicas != rv.nr_replicas {
            return false;
        }

        l.k.resize(l.k.size + r.k.size);
        true
    }

    const TRIGGER: Option<&'static Trigger> = Some(&crate::alloc::buckets::RESERVATION);
}
