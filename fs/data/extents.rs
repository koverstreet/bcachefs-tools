//! Extents: keys with a size, a range of their btree's offsets - what's
//! cut, merged and counted as file data. Their pointers, where they have
//! them, are data/ptrs.rs's.

use crate::bkey_fsck_err_on;
use crate::btree::bkey::{bkey_family, bkey_ge, bkey_gt, bkey_le, bkey_lt, BkeyS, BkeySC, BkeyV, BkeyVMut};
use crate::c;
use crate::data::ptrs::{crc_is_compressed, crc_unpack, BkeyVPtrs, ExtentEntry, ExtentEntryMut, Ptrs};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::util::ffi::Opaque;
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::Printbuf;
use core::ffi::c_int;
use cstruct_macros::c_export;

/*
 * BKEY_TYPE_extent flags the key types an extents btree may hold - checked
 * against BCH_BTREE_IDS(), which is what says: deleted, which every btree
 * may, and what each BTREE_IS_extents btree allows.
 */
macro_rules! extents_btrees_key_types {
    ($(($name:tt, $nr:literal, $($flag:ident)|+, $(BIT_ULL($t:ident))|+ $(, $($rest:tt)*)?)),* $(,)?) => {
        const EXTENTS_BTREES_KEY_TYPES: u64 =
            1 << c::bch_bkey_type::KEY_TYPE_deleted.0 $($(| 1 << c::bch_bkey_type::$t.0)+)*;
    };
}
c::BCH_BTREE_IDS!(cstruct_macros::xmacro_select [BTREE_IS_extents => extents_btrees_key_types]);

macro_rules! extent_types {
    ($(($name:ident, $nr:literal, $($rest:tt)*)),* $(,)?) => {
        const EXTENT_KEY_TYPES: u64 = 0 $(| 1 << $nr)*;
    };
}
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_extent => extent_types]);

bkey_family!(Extents);

const _: () = assert!(EXTENT_KEY_TYPES == EXTENTS_BTREES_KEY_TYPES,
                      "BKEY_TYPE_extent isn't on exactly the key types BCH_BTREE_IDS()' extents btrees hold");

impl c::bkey {
    /// Whether it's counted in its inode's i_sectors.
    pub fn extent_is_allocation(&self) -> bool {
        use c::bch_bkey_type as t;

        matches!(self.key_type(),
                 t::KEY_TYPE_extent | t::KEY_TYPE_reservation | t::KEY_TYPE_reflink_p |
                 t::KEY_TYPE_reflink_v | t::KEY_TYPE_inline_data |
                 t::KEY_TYPE_indirect_inline_data | t::KEY_TYPE_error)
    }

    /// Whether it's data a file reads: pointers to it, the data inline, or a
    /// reflink_p to it - bkey_extent_is_data().
    pub fn extent_is_data(&self) -> bool {
        use c::bch_bkey_type as t;

        self.extent_is_direct_data() ||
            matches!(self.key_type(),
                     t::KEY_TYPE_inline_data | t::KEY_TYPE_indirect_inline_data | t::KEY_TYPE_reflink_p)
    }

    /// Whether its value is pointers to its data, and not to another key's.
    pub fn extent_is_direct_data(&self) -> bool {
        use c::bch_bkey_type as t;

        matches!(self.key_type(),
                 t::KEY_TYPE_btree_ptr | t::KEY_TYPE_btree_ptr_v2 | t::KEY_TYPE_extent |
                 t::KEY_TYPE_reflink_v)
    }
}

impl BkeySC<'_> {
    /// Whether it has space allocated and not yet written: an unwritten
    /// pointer.
    pub fn extent_is_unwritten(self, fs: &Fs) -> bool {
        self.arm::<Ptrs>().is_some_and(|k| k.ptrs(fs).any(|p| p.unwritten() != 0))
    }

    /// Whether it reserves space rather than holding data - a reservation, or
    /// an extent with unwritten pointers.
    pub fn extent_is_reservation(self, fs: &Fs) -> bool {
        self.k.key_type() == c::bch_bkey_type::KEY_TYPE_reservation || self.extent_is_unwritten(fs)
    }

}

/*
 * A cut that leaves nothing - which makes the key a size 0 deletion - gives
 * back None, not an empty extent: what's left isn't to be inserted, and the
 * caller branches on it. One that leaves anything leaves its type, so it's
 * still an extent.
 */

/// What an extent, armed to Extents, does in whichever form it's held.
pub trait BkeyVExtents: BkeyV {
    /// Cut to start at @where_: bch2_cut_front_s().
    fn cut_front(mut self, fs: &Fs, where_: c::bpos) -> Option<Self>
    where Self: BkeyVMut<V = Extents>
    {
        self.cut_front_in_place(fs, where_);
        (self.k().size != 0).then_some(self)
    }

    /// Cut to end at @where_: bch2_cut_back_s().
    fn cut_back(mut self, where_: c::bpos) -> Option<Self>
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
    fn cut_front_in_place(&mut self, fs: &Fs, where_: c::bpos)
    where Self: BkeyVMut<V = Extents>
    {
        use c::bch_bkey_type as t;

        let mut k = self.as_s().untyped();
        if bkey_le(where_, k.k.start_pos()) {
            return;
        }
        debug_assert!(!bkey_gt(where_, k.k.p), "cutting the front of an extent past its end");

        // as C's: a u64 into a u32 size, wrapping
        let sub = where_.offset.wrapping_sub(k.k.start_offset());
        k.k.size = k.k.size.wrapping_sub(sub as u32);

        let mut new_val_u64s = k.k.val_bytes() / size_of::<u64>();
        if k.k.size == 0 {
            k.k.type_ = t::KEY_TYPE_deleted.0 as u8;
            new_val_u64s = 0;
        }

        match k.k.key_type() {
            t::KEY_TYPE_extent | t::KEY_TYPE_reflink_v => {
                let mut p = k.as_s().arm::<Ptrs>().expect("an extent has pointers");
                // a pointer after a crc points to the crc's data, whose
                // offset moves instead
                let mut seen_crc = false;
                let pk = *p.k();

                for e in p.extent_entries_mut(fs) {
                    match e {
                        ExtentEntryMut::ptr(ptr) => if !seen_crc {
                            ptr.set_offset(ptr.offset() + sub);
                        },
                        mut e => if let Some(mut crc) = crc_unpack(&pk, e.as_entry()) {
                            crc.offset = (u64::from(crc.offset) + sub) as u16;
                            e.set_crc(&crc).expect("a crc's offset, moved within its data, fits its entry");
                            seen_crc = true;
                        },
                    }
                }
            }
            t::KEY_TYPE_reflink_p => {
                let mut p = k.as_s().arm::<c::ReflinkP>().expect("a reflink_p");
                let v = p.v_mut();
                v.set_idx(v.idx() + sub);
            }
            ty @ (t::KEY_TYPE_inline_data | t::KEY_TYPE_indirect_inline_data) => {
                let off = inline_data_offset(ty);
                let val = k.val_bytes_mut();
                let bytes = val.len() - off;
                let sub = (sub << 9).min(bytes as u64) as usize;

                val.copy_within(off + sub.., off);
                new_val_u64s -= sub / size_of::<u64>();
            }
            _ => {}
        }

        shrink_val(k, new_val_u64s);
    }

    /// Cut to end at @where_, if it ends after it - inline data past it
    /// dropped - and a cut that leaves nothing makes it a deletion. The u64s
    /// its value gives up are zeroed. bch2_cut_back_s().
    fn cut_back_in_place(&mut self, where_: c::bpos)
    where Self: BkeyVMut<V = Extents>
    {
        use c::bch_bkey_type as t;

        let k = self.as_s().untyped();
        if bkey_ge(where_, k.k.p) {
            return;
        }
        debug_assert!(!bkey_lt(where_, k.k.start_pos()), "cutting the back of an extent before its start");

        let len = where_.offset.wrapping_sub(k.k.start_offset());
        k.k.p.offset = where_.offset;
        k.k.size = len as u32;

        let mut new_val_u64s = k.k.val_bytes() / size_of::<u64>();
        if len == 0 {
            k.k.type_ = t::KEY_TYPE_deleted.0 as u8;
            new_val_u64s = 0;
        }

        if let ty @ (t::KEY_TYPE_inline_data | t::KEY_TYPE_indirect_inline_data) = k.k.key_type() {
            let off = inline_data_offset(ty);
            let bytes = k.k.val_bytes() - off;
            new_val_u64s = (off + bytes.min((k.k.size as usize) << 9)) / size_of::<u64>();
        }

        shrink_val(k, new_val_u64s);
    }
}

impl<K: BkeyV> BkeyVExtents for K {}

/// Where an inline data key's data starts in its value - after an indirect
/// one's refcount: bkey_inline_data_offset().
fn inline_data_offset(ty: c::bch_bkey_type) -> usize {
    match ty {
        c::bch_bkey_type::KEY_TYPE_inline_data          => size_of::<c::InlineData>(),
        c::bch_bkey_type::KEY_TYPE_indirect_inline_data => size_of::<c::IndirectInlineData>(),
        _ => unreachable!("inline data offset of a {ty:?}"),
    }
}

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
#[c_export]
pub fn bch2_cut_front_s(c: &Fs, where_: c::bpos, k: BkeyS<'_>) -> c_int {
    let u64s = c_int::from(k.k.u64s);
    let mut k = k.arm::<Extents>().expect("cutting a key an extents btree can't hold");
    k.cut_front_in_place(c, where_);
    c_int::from(k.k().u64s) - u64s
}

/// The change in @k's u64s, never above 0.
#[c_export]
pub fn bch2_cut_back_s(where_: c::bpos, k: BkeyS<'_>) -> c_int {
    let u64s = c_int::from(k.k.u64s);
    let mut k = k.arm::<Extents>().expect("cutting a key an extents btree can't hold");
    k.cut_back_in_place(where_);
    c_int::from(k.k().u64s) - u64s
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
            c::bch_csum_type(lc.csum_type.into()).mergeable() &&
            u32::from(lc.offset) + lc.live_size == lc.compressed_size &&
            rc.offset == 0 &&
            (lc.csum_type == 0 ||
             lc.uncompressed_size + rc.uncompressed_size <= fs.opts().encoded_extent_max >> 9),
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
                // SAFETY: plain values. The csum type is mergeable, as
                // bch2_checksum_merge() BUG()s if it isn't: ptrs_mergeable()
                // checked it, with the pointer after this entry - validate
                // has one after every crc entry
                cl.csum = unsafe { c::bch2_checksum_merge(cl.csum_type.into(), cl.csum, cr.csum,
                                                          (cr.uncompressed_size << 9) as usize) };
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

#[c_export]
pub fn bch2_extent_merge(c: &Opaque<c::bch_fs>, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
    extent_merge(Fs::from_c(c), l, r)
}

// ── reservations ─────────────────────────────────────────────────────────

fn reservation_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let r = v.k.arm::<c::Reservation>().expect("a reservation").v();

    bkey_fsck_err_on!(v, r.nr_replicas == 0 || u32::from(r.nr_replicas) > c::BCH_REPLICAS_MAX,
                      id::reservation_key_nr_replicas_invalid,
                      "invalid nr_replicas ({})", r.nr_replicas)
}

#[c_export]
pub fn bch2_reservation_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> c_int
{
    ret_to_c(reservation_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_reservation_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let r = k.arm::<c::Reservation>().expect("a reservation").v();

    write!(out, "generation {} replicas {}", r.generation.get(), r.nr_replicas);
}

/// Adjacent reservations of the same generation and replicas are one.
#[c_export]
pub fn bch2_reservation_merge(_c: &Opaque<c::bch_fs>, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
    let lv = l.as_sc().arm::<c::Reservation>().expect("a reservation").v();
    let rv = r.arm::<c::Reservation>().expect("a reservation").v();

    if lv.generation.get() != rv.generation.get() || lv.nr_replicas != rv.nr_replicas {
        return false;
    }

    l.k.resize(l.k.size + r.k.size);
    true
}
