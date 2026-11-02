// SPDX-License-Identifier: GPL-2.0

/* Auxiliary search trees */

use super::bset::{AuxTreeType, BsetTree, BSET_CACHELINE, BSET_NO_AUX_TREE_VAL, BSET_RW_AUX_TREE_VAL};
use super::keys::{bkey_p_next, key_at, set_keys, Keys, KeysMut};
use super::{check_lookups, BtreeNode};
use crate::btree::bkey::{BkeyP, HIGH_BIT_OFFSET};
use crate::c;
use crate::format::Bpos;
use crate::types::{L1_CACHE_BYTES, SMP_CACHE_BYTES};
use crate::util::eytzinger::Eytzinger1;
use crate::util::kernel::{likely, prefetch, unlikely};
use crate::util::vstructs::{cast_plain_slice, cast_plain_slice_mut, plain_bytes, plain_bytes_mut, Plain};
use cstruct_macros::Plain;

const BFLOAT_FAILED_UNPACKED: u8 = u8::MAX;
pub(super) const BFLOAT_FAILED: u8 = u8::MAX;

#[repr(C)]
#[derive(Clone, Copy, Default, Plain)]
pub struct BkeyFloat {
    pub(super) exponent:   u8,
    pub(super) key_offset: u8,
    pub(super) mantissa:   u16,
}
const BKEY_MANTISSA_BITS: u32 = 16;

#[repr(C)]
#[derive(Clone, Copy, Default, Plain)]
pub struct RwAuxEntry {
    pub(super) offset: u16,
    _pad:              u16,
    pub(super) k:      Bpos,
}

/// A set's lookup table, as its extra says.
pub enum AuxTree<'a> {
    None,
    Ro(RoAuxTree<'a>),
    Rw(&'a [RwAuxEntry]),
}

#[derive(Clone, Copy)]
pub struct RoAuxTree<'a> {
    pub(super) eytz: Eytzinger1<usize>,
    pub(super) f:    &'a [BkeyFloat],
}

const L1_CACHE_U64S: usize = L1_CACHE_BYTES / size_of::<u64>();
const BSET_CACHELINE_U64S: usize = BSET_CACHELINE / size_of::<u64>();

/// Set @t's table in @aux, t.size entries.
fn aux_slice<'a, T: Plain>(aux: &'a [u64], t: &BsetTree) -> &'a [T] {
    let off = t.aux_data_offset as usize;
    invariant!(off <= aux.len());

    let rest = plain_bytes(&aux[off..]);
    let bytes = t.size as usize * size_of::<T>();
    invariant!(bytes <= rest.len());

    cast_plain_slice(&rest[..bytes]).expect("a u64 aligned aux tree")
}

/// Set @t's table in @aux, to its end: as many entries as the last set's can
/// grow to.
fn aux_slice_mut<'a, T: Plain>(aux: &'a mut [u64], t: &BsetTree) -> &'a mut [T] {
    let bytes = plain_bytes_mut(&mut aux[t.aux_data_offset as usize..]);
    let len = bytes.len() / size_of::<T>() * size_of::<T>();
    cast_plain_slice_mut(&mut bytes[..len]).expect("a u64 aligned aux tree")
}

impl BsetTree {
    fn eytzinger(&self) -> Eytzinger1<usize> {
        invariant!(self.size >= 2);
        Eytzinger1::<usize>::with_extra(self.size as usize - 1, self.extra as usize)
    }

    fn aux_tree_buf_end(&self) -> usize {
        ebug_assert!(self.aux_data_offset != u16::MAX);

        let bytes = match self.aux_tree_type() {
            AuxTreeType::BSET_NO_AUX_TREE => 0,
            AuxTreeType::BSET_RO_AUX_TREE => self.size as usize * size_of::<BkeyFloat>(),
            AuxTreeType::BSET_RW_AUX_TREE => self.size as usize * size_of::<RwAuxEntry>(),
        };
        self.aux_data_offset as usize + bytes.div_ceil(8)
    }
}

/* Binary tree stuff for auxiliary search trees */

/*
 * Cacheline/offset <-> bkey pointer arithmetic:
 *
 * t->tree is a binary search tree in an array; each node corresponds to a key
 * in one cacheline in t->set (BSET_CACHELINE bytes).
 *
 * This means we don't have to store the full index of the key that a node in
 * the binary tree points to; eytzinger1_to_inorder() gives us the cacheline, and
 * then bkey_float->m gives us the offset within that cacheline, in units of 8
 * bytes.
 *
 * cacheline_to_bkey() and friends abstract out all the pointer arithmetic to
 * make this work.
 *
 * To construct the bfloat for an arbitrary key we need to know what the key
 * immediately preceding it is: we have to check if the two keys differ in the
 * bits we're going to store in bkey_float->mantissa. t->prev[j] stores the size
 * of the previous key so we can walk backwards to it from t->tree[j]'s key.
 */

impl BsetTree {
    fn bset_cacheline(&self, cacheline: usize) -> usize {
        (self.first_offset() as usize & !(L1_CACHE_U64S - 1)) + cacheline * BSET_CACHELINE_U64S
    }

    fn cacheline_to_bkey(&self, cacheline: usize, offset: u8) -> u16 {
        (self.bset_cacheline(cacheline) + offset as usize) as u16
    }

    pub(super) fn bkey_to_cacheline(&self, k: u16) -> usize {
        (k as usize - self.bset_cacheline(0)) / BSET_CACHELINE_U64S
    }

    fn bkey_to_cacheline_offset(&self, cacheline: usize, k: u16) -> u8 {
        let m = k as usize - self.bset_cacheline(cacheline);

        ebug_assert!(m <= u8::MAX as usize);
        m as u8
    }
}

impl RoAuxTree<'_> {
    #[inline]
    pub(super) fn tree_to_bkey(&self, t: &BsetTree, j: usize) -> u16 {
        invariant!(j != 0, j <= self.eytz.size(), j < self.f.len());
        t.cacheline_to_bkey(self.eytz.to_inorder(j), self.f[j].key_offset)
    }
}

/*
 * For the write set - the one we're currently inserting keys into - we don't
 * maintain a full search tree, we just keep a simple lookup table in t->prev.
 */
fn rw_aux_tree_entry(node: *const BtreeNode, buf: &[u64], t: &BsetTree, k: u16) -> RwAuxEntry {
    ebug_assert!(k < t.end_offset);

    RwAuxEntry {
        offset: k,
        _pad:   0,
        k:      unsafe { key_at(buf, k).unpack_pos_raw(node) },
    }
}

impl<'a> Keys<'a> {
    #[inline]
    pub fn aux_tree(&self, t: &BsetTree) -> AuxTree<'a> {
        match t.aux_tree_type() {
            AuxTreeType::BSET_NO_AUX_TREE => AuxTree::None,
            AuxTreeType::BSET_RO_AUX_TREE => AuxTree::Ro(RoAuxTree { eytz: t.eytzinger(), f: aux_slice(self.aux, t) }),
            AuxTreeType::BSET_RW_AUX_TREE => AuxTree::Rw(aux_slice(self.aux, t)),
        }
    }

    fn aux_tree_buf_start(&self, set: usize) -> usize {
        if set == 0 {
            (self.unpack_fn_len as usize).div_ceil(8)
        } else {
            self.sets[set - 1].aux_tree_buf_end()
        }
    }

    fn __bset_verify_rw_aux_tree(&self, t: &BsetTree) {
        let mut k = t.first_offset();
        let mut j = 0;

        assert!(!t.has_ro_aux_tree(), "rw aux tree check of a set with a ro aux tree");

        let AuxTree::Rw(rw) = self.aux_tree(t) else { return };

        assert!(!rw.is_empty(), "rw aux tree of no entries");
        assert!(rw[0].offset == k, "rw aux tree's first entry at {}, its set's first key at {k}", rw[0].offset);

        loop {
            j += 1;
            if j == rw.len() {
                break;
            }

            assert!(rw[j].offset > rw[j - 1].offset,
                    "rw aux tree entry {j} at {}, not past entry {} at {}", rw[j].offset, j - 1, rw[j - 1].offset);

            while rw[j].offset != k {
                k = self.bkey_p_next(k);
                assert!(k < t.end_offset, "rw aux tree entry {j} at {}, not at a key of its set", rw[j].offset);
            }

            assert!(rw[j].k == self.unpack_pos(self.key_at(k)),
                    "rw aux tree entry {j} at {}: {}, its key at {}", k, rw[j].k, self.unpack_pos(self.key_at(k)));
        }
    }

    pub(super) fn bset_verify_rw_aux_tree(&self, t: &BsetTree) {
        if check_lookups() {
            self.__bset_verify_rw_aux_tree(t);
        }
    }

    fn __bset_aux_tree_verify_ro(&self, t: &BsetTree, ro: RoAuxTree<'_>) {
        let mut k = t.first_offset();

        for j in ro.eytz.iter() {
            while ro.tree_to_bkey(t, j) > k && k != t.end_offset {
                k = self.bkey_p_next(k);
            }

            assert!(ro.tree_to_bkey(t, j) == k,
                    "ro aux tree node {j} at {}, not at a key of its set", ro.tree_to_bkey(t, j));
        }
    }

    fn __bset_aux_tree_verify(&self) {
        let aux_u64s = self.aux.len();

        for (i, t) in self.sets.iter().enumerate() {
            if t.aux_data_offset == u16::MAX {
                continue;
            }

            assert!(i == 0 || self.sets[i - 1].aux_data_offset != u16::MAX,
                    "set {i} has an aux tree, set {} doesn't", i - 1);

            let (offset, start, end) = (t.aux_data_offset as usize, self.aux_tree_buf_start(i), t.aux_tree_buf_end());
            assert!(offset >= start, "set {i}'s aux tree at {offset}, before the end of the last at {start}");
            assert!(offset <= aux_u64s, "set {i}'s aux tree at {offset}, past the aux data's {aux_u64s} u64s");
            assert!(end <= aux_u64s, "set {i}'s aux tree ends at {end}, past the aux data's {aux_u64s} u64s");

            match self.aux_tree(t) {
                AuxTree::Ro(ro) => self.__bset_aux_tree_verify_ro(t, ro),
                AuxTree::Rw(_)  => self.__bset_verify_rw_aux_tree(t),
                AuxTree::None   => {}
            }
        }
    }

    pub(super) fn bset_aux_tree_verify(&self) {
        if check_lookups() {
            self.__bset_aux_tree_verify();
        }
    }
}

/* returns idx of first entry >= offset: */
pub(super) fn rw_aux_tree_bsearch(t: &BsetTree, rw: &[RwAuxEntry], offset: u16) -> usize {
    let bset_offs = (offset - t.first_offset()) as usize;
    let bset_u64s = (t.end_offset - t.first_offset()) as usize;
    let mut idx = if bset_u64s != 0 { bset_offs * rw.len() / bset_u64s } else { 0 };

    ebug_assert!(t.aux_tree_type() == AuxTreeType::BSET_RW_AUX_TREE);
    ebug_assert!(!rw.is_empty());
    ebug_assert!(idx <= rw.len());

    while idx < rw.len() && rw[idx].offset < offset {
        idx += 1;
    }

    while idx > 0 && rw[idx - 1].offset >= offset {
        idx -= 1;
    }

    ebug_assert!(idx == rw.len() || rw[idx].offset >= offset);
    ebug_assert!(idx == 0 || rw[idx - 1].offset < offset);
    ebug_assert!(idx + 1 >= rw.len() || rw[idx].offset != rw[idx + 1].offset);

    idx
}

/*
 * In little endian, we're shifting off low bits (and then the bits we
 * want are at the low end), in big endian we're shifting off high bits
 * (and then the bits we want are at the high end, so we shift them
 * back down):
 */
fn bkey_mantissa_shift(v: u64, f: &BkeyFloat) -> u16 {
    if cfg!(target_endian = "little") {
        (v >> (f.exponent & 7)) as u16
    } else {
        (v >> (64 - (f.exponent & 7) as u32 - BKEY_MANTISSA_BITS)) as u16
    }
}

/// The unaligned u64 at byte @at of @bytes: get_unaligned().
fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(bytes[at..at + size_of::<u64>()].try_into().unwrap())
}

/// Of a key in the node's buffer, at @k.
fn bkey_mantissa(buf: &[u64], k: u16, f: &BkeyFloat) -> u16 {
    ebug_assert!(key_at(buf, k).is_packed());

    let at = k as usize * size_of::<u64>() + (f.exponent >> 3) as usize;
    bkey_mantissa_shift(read_u64(plain_bytes(buf), at), f)
}

/// The 8 bytes from a u8 exponent's byte are within a bkey_packed, so the
/// search key's mantissa is read without a bounds check.
const _: () = assert!((u8::MAX >> 3) as usize + size_of::<u64>() <= size_of::<c::bkey_packed>());

/// Of the search key, packed onto the stack.
fn bkey_mantissa_search(k: &c::bkey_packed, f: &BkeyFloat) -> u16 {
    bkey_mantissa_shift(read_u64(plain_bytes(k), (f.exponent >> 3) as usize), f)
}

/// @pos packed, if it packs, or unpacked: a bound for make_bfloat().
fn pack_pos_or_unpacked(node: *const BtreeNode, pos: Bpos) -> c::bkey_packed_padded {
    let mut k = c::bkey_packed_padded::default();

    if !unsafe { c::bch2_bkey_pack_pos(&mut k.k, pos, node) } {
        let u = c::bkey { p: pos, ..c::bkey::new() };
        plain_bytes_mut(&mut k.k).copy_from_slice(plain_bytes(&u));
    }
    k
}

impl KeysMut<'_> {
    fn make_bfloat(&mut self, set: usize, j: usize, min_key: &BkeyP, max_key: &BkeyP) {
        let t = self.sets[set];
        let f: &mut [BkeyFloat] = aux_slice_mut(self.aux, &t);
        let ro = RoAuxTree { eytz: t.eytzinger(), f: &*f };

        let m = ro.tree_to_bkey(&t, j);
        let l = if j.is_power_of_two() {
            min_key
        } else {
            key_at(self.buf, ro.tree_to_bkey(&t, j >> (j.trailing_zeros() + 1)))
        };
        let r = if (j + 1).is_power_of_two() {
            max_key
        } else {
            key_at(self.buf, ro.tree_to_bkey(&t, j >> (j.trailing_ones() + 1)))
        };

        /*
         * for failed bfloats, the lookup code falls back to comparing against
         * the original key.
         */

        if !l.is_packed() || !r.is_packed() || !key_at(self.buf, m).is_packed() ||
           self.nr_key_bits == 0 {
            f[j].exponent = BFLOAT_FAILED_UNPACKED;
            return;
        }

        let nr_key_bits = self.nr_key_bits as i32;
        let key_u64s = self.format.key_u64s as i32;

        /*
         * The greatest differing bit of l and r is the first bit we must
         * include in the bfloat mantissa we're creating in order to do
         * comparisons - that bit always becomes the high bit of
         * bfloat->mantissa, and thus the exponent we're calculating here is
         * the position of what will become the low bit in bfloat->mantissa:
         *
         * Note that this may be negative - we may be running off the low end
         * of the key: we handle this later:
         */
        let differing = unsafe { c::bch2_bkey_greatest_differing_bit(self.node, l.as_ffi(), r.as_ffi()) };
        let high_bit = (differing as i32).max(nr_key_bits.min(BKEY_MANTISSA_BITS as i32) - 1);
        let exponent = high_bit - (BKEY_MANTISSA_BITS as i32 - 1);

        /*
         * Then we calculate the actual shift value, from the start of the key
         * (k->_data), to get the key bits starting at exponent:
         */
        let shift = if cfg!(target_endian = "little") {
            let shift = (key_u64s * 64 - nr_key_bits) + exponent;

            ebug_assert!(shift + BKEY_MANTISSA_BITS as i32 <= key_u64s * 64);
            shift
        } else {
            let shift = HIGH_BIT_OFFSET as i32 + nr_key_bits - exponent - BKEY_MANTISSA_BITS as i32;

            ebug_assert!(shift >= c::KEY_PACKED_BITS_START as i32);
            shift
        };
        ebug_assert!(shift >= 0 && shift < BFLOAT_FAILED as i32);

        f[j].exponent = shift as u8;
        let mut mantissa = bkey_mantissa(self.buf, m, &f[j]) as u32;

        /*
         * If we've got garbage bits, set them to all 1s - it's legal for the
         * bfloat to compare larger than the original key, but not smaller:
         */
        if exponent < 0 {
            mantissa |= !(!0u32 << -exponent);
        }

        f[j].mantissa = mantissa as u16;
    }

    /* bytes remaining - only valid for last bset: */
    fn __bset_tree_capacity(&self, set: usize) -> usize {
        self.as_keys().bset_aux_tree_verify();

        (self.aux.len() - self.sets[set].aux_data_offset as usize) * size_of::<u64>()
    }

    #[inline(never)]
    fn __build_rw_aux_tree(&mut self, set: usize) {
        let capacity = self.__bset_tree_capacity(set) / size_of::<RwAuxEntry>();
        let t = &mut self.sets[set];
        let rw: &mut [RwAuxEntry] = aux_slice_mut(self.aux, t);

        t.size = 1;
        t.extra = BSET_RW_AUX_TREE_VAL;
        rw[0].offset = t.first_offset();

        for (k, _) in set_keys(self.buf, t) {
            if t.size as usize == capacity {
                break;
            }

            if (k - rw[t.size as usize - 1].offset) as usize * size_of::<u64>() > L1_CACHE_BYTES {
                rw[t.size as usize] = rw_aux_tree_entry(self.node, self.buf, t, k);
                t.size += 1;
            }
        }
    }

    #[inline(never)]
    fn __build_ro_aux_tree(&mut self, set: usize) {
        let capacity = self.__bset_tree_capacity(set) / size_of::<BkeyFloat>();
        let t = &mut self.sets[set];
        let f: &mut [BkeyFloat] = aux_slice_mut(self.aux, t);

        t.size = t.bkey_to_cacheline(t.end_offset).min(capacity) as u16;
        'retry: loop {
            if t.size < 2 {
                t.size = 0;
                t.extra = BSET_NO_AUX_TREE_VAL;
                return;
            }

            t.extra = Eytzinger1::<usize>::new(t.size as usize - 1).extra() as u16;

            /* First we figure out where the first key in each cacheline is */
            let mut k = t.first_offset();
            let mut cacheline = 1;

            for j in t.eytzinger().iter() {
                while t.bkey_to_cacheline(k) < cacheline {
                    k = bkey_p_next(self.buf, k);
                }

                if k >= t.end_offset {
                    /* XXX: this path sucks */
                    t.size -= 1;
                    continue 'retry;
                }

                f[j].key_offset = t.bkey_to_cacheline_offset(cacheline, k);
                cacheline += 1;

                ebug_assert!(RoAuxTree { eytz: t.eytzinger(), f: &*f }.tree_to_bkey(t, j) == k);
            }
            break;
        }

        let eytz = t.eytzinger();
        let header = self.as_keys().header();
        let min_key = pack_pos_or_unpacked(self.node, header.min_key);
        let max_key = pack_pos_or_unpacked(self.node, header.max_key);
        let min_key = BkeyP::from_packed(&min_key.k).expect("a packed position").cast();
        let max_key = BkeyP::from_packed(&max_key.k).expect("a packed position").cast();

        /* Then we build the tree */
        for j in eytz.iter() {
            self.make_bfloat(set, j, min_key, max_key);
        }
    }

    fn bset_alloc_tree(&mut self, set: usize) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            for (i, t) in self.sets[..set].iter().enumerate() {
                assert!(!t.has_rw_aux_tree(), "set {i} has a rw aux tree, before set {set}'s");
            }
        }

        self.sets.set_no_aux_tree(set);

        /* round up to next cacheline: */
        let start = self.as_keys().aux_tree_buf_start(set);
        self.sets[set].aux_data_offset = start.next_multiple_of(SMP_CACHE_BYTES / size_of::<u64>()) as u16;

        self.as_keys().bset_aux_tree_verify();
    }

    pub fn bset_build_aux_tree(&mut self, set: usize, writeable: bool) {
        let t = &self.sets[set];
        let has = if writeable { t.has_rw_aux_tree() } else { t.has_ro_aux_tree() };
        if has {
            return;
        }

        self.bset_alloc_tree(set);

        if self.__bset_tree_capacity(set) == 0 {
            return;
        }

        if writeable {
            self.__build_rw_aux_tree(set);
        } else {
            self.__build_ro_aux_tree(set);
        }

        self.as_keys().bset_aux_tree_verify();
    }

    /* Insert */

    fn rw_aux_tree_insert_entry(&mut self, set: usize, idx: usize) {
        let t = &mut self.sets[set];
        let rw: &mut [RwAuxEntry] = aux_slice_mut(self.aux, t);
        let size = t.size as usize;

        ebug_assert!(idx != 0 && idx <= size);
        let start = rw[idx - 1].offset;
        let end = if idx < size { rw[idx].offset } else { t.end_offset };

        if size < rw.len() &&
           (end - start) as usize * size_of::<u64>() > L1_CACHE_BYTES {
            let mut k = start;

            loop {
                k = bkey_p_next(self.buf, k);
                if k == end {
                    break;
                }

                if (k - start) as usize * size_of::<u64>() >= L1_CACHE_BYTES {
                    rw.copy_within(idx..size, idx + 1);
                    t.size += 1;
                    rw[idx] = rw_aux_tree_entry(self.node, self.buf, t, k);
                    break;
                }
            }
        }
    }

    fn __bset_fix_lookup_table(&mut self, set: usize, where_: u16, clobber_u64s: u16, new_u64s: u16) {
        let shift = new_u64s as i32 - clobber_u64s as i32;
        let t = &mut self.sets[set];
        let rw: &mut [RwAuxEntry] = aux_slice_mut(self.aux, t);

        ebug_assert!(!t.has_ro_aux_tree());

        if where_ > rw[t.size as usize - 1].offset {
            let size = t.size as usize;
            self.rw_aux_tree_insert_entry(set, size);
            return;
        }

        /* returns first entry >= where */
        let mut idx = rw_aux_tree_bsearch(t, &rw[..t.size as usize], where_);

        if rw[idx].offset == where_ {
            if idx == 0 { /* never delete first entry */
                idx += 1;
            } else if where_ < t.end_offset {
                rw[idx] = rw_aux_tree_entry(self.node, self.buf, t, where_);
                idx += 1;
            } else {
                ebug_assert!(where_ == t.end_offset);
                t.size -= 1;
                let size = t.size as usize;
                self.rw_aux_tree_insert_entry(set, size);
                return;
            }
        }

        ebug_assert!(idx == t.size as usize || rw[idx].offset > where_);
        if idx < t.size as usize &&
           rw[idx].offset as i32 + shift == rw[idx - 1].offset as i32 {
            rw.copy_within(idx + 1..t.size as usize, idx);
            t.size -= 1;
        }

        for e in &mut rw[idx..t.size as usize] {
            e.offset = (e.offset as i32 + shift) as u16;
        }

        ebug_assert!(idx == t.size as usize || rw[idx].offset != rw[idx - 1].offset);

        self.rw_aux_tree_insert_entry(set, idx);
    }

    pub(super) fn bset_fix_lookup_table(&mut self, set: usize, where_: u16, clobber_u64s: u16, new_u64s: u16) {
        if self.sets[set].has_rw_aux_tree() {
            self.__bset_fix_lookup_table(set, where_, clobber_u64s, new_u64s);
            self.as_keys().bset_verify_rw_aux_tree(&self.sets[set]);
            self.as_keys().bset_aux_tree_verify();
        }
    }
}

/* Lookup */

pub(super) fn bset_search_write_set(rw: &[RwAuxEntry], search: &Bpos) -> u16 {
    /*
     * hey, we can use this when stdlib people fix their shit: its loop is
     * branchless and has no bounds checks, but it emits the compare twice
     * (once peeled after the loop), and its result isn't known to be in
     * bounds, so the index is still checked:
     *
     * invariant!(!rw.is_empty());
     * rw[rw[1..].partition_point(|e| e.k < *search)].offset
     */
    let (mut l, mut r) = (0, rw.len());

    while l + 1 != r {
        let m = (l + r) >> 1;

        if rw[m].k < *search {
            l = m;
        } else {
            r = m;
        }
    }

    rw[l].offset
}

fn bkey_mantissa_bits_dropped(b: &BtreeNode, f: &BkeyFloat) -> bool {
    invariant!(b.format.key_u64s as u32 * 64 >= b.nr_key_bits as u32);

    if cfg!(target_endian = "little") {
        let key_bits_start = b.format.key_u64s as u32 * 64 - b.nr_key_bits as u32;

        f.exponent as u32 > key_bits_start
    } else {
        let key_bits_end = HIGH_BIT_OFFSET + b.nr_key_bits as u32;

        (f.exponent as u32 + BKEY_MANTISSA_BITS) < key_bits_end
    }
}

pub(super) fn bset_search_tree(keys: &Keys<'_>, t: &BsetTree, ro: RoAuxTree<'_>,
                               search: &Bpos, packed_search: &c::bkey_packed) -> u16 {
    let b = keys.node();
    let packed: &BkeyP = BkeyP::from_packed(packed_search).expect("a packed search key").cast();
    /* t.size bounds it, so n * 2 can't overflow: the optimizer sees that */
    let base = &ro.f[..t.size as usize];
    /* a ro tree has at least two nodes: the loop runs at least once, as C's do-while */
    let mut f = &base[1];
    let mut n = 1;

    while n < base.len() {
        if likely(n << 4 < base.len()) {
            prefetch(base.as_ptr().wrapping_add(n << 4));
        }

        f = &base[n];
        n = n * 2 + 'dir: {
            if likely(f.exponent < BFLOAT_FAILED) {
                let l = f.mantissa;
                let r = bkey_mantissa_search(packed_search, f);

                if likely(l != r) || !bkey_mantissa_bits_dropped(b, f) {
                    break 'dir (l < r) as usize;
                }
            }

            let k = ro.tree_to_bkey(t, n);
            let cmp = b.cmp_p_or_unp(keys.key_at(k), Some(packed), search);
            if cmp.is_eq() {
                return k;
            }

            cmp.is_lt() as usize
        };
    }

    let mut inorder = ro.eytz.to_inorder(n >> 1);

    /*
     * n would have been the node we recursed to - the low bit tells us if
     * we recursed left or recursed right.
     */
    if likely(n & 1 == 0) {
        inorder -= 1;
        if unlikely(inorder == 0) {
            return t.first_offset();
        }

        f = &base[ro.eytz.prev(n >> 1)];
    }

    t.cacheline_to_bkey(inorder, f.key_offset)
}
