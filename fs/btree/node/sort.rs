// SPDX-License-Identifier: GPL-2.0

use super::bset::{BsetTree, MAX_BSETS};
use super::iter::KeyIter;
use super::keys::{key_at, keys_in, Keys};
use super::{BtreeNode, BtreeNrKeys};
use crate::btree::bkey::{BkeyP, BKEY_U64S};
use crate::btree::cache::Bounce;
use crate::c;
use crate::fs::Fs;
use crate::types::{c_default, Le64};
use crate::util::kernel::{copy_u64s_small, local_clock, mem_alloc_profiling_enabled};
use crate::util::time_stats::TimeStats;
use crate::util::vstructs::{plain_bytes, Head};
use arrayvec::ArrayVec;
use core::cmp::Ordering;
use core::ffi::c_uint;
use core::mem::offset_of;
use cstruct_macros::{c_enum, c_export, c_name, CStruct};

/// [k, end), offsets into the node's buffer: u32s, as the unwritten whiteouts
/// run to its end, past a 512k node's last u16.
#[repr(C)]
#[c_name(sort_iter_set)]
#[derive(Clone, Copy, CStruct)]
pub struct SortIterSet {
    pub k:   u32,
    pub end: u32,
}
c_default!(SortIterSet);

#[repr(C)]
#[c_name(sort_iter)]
#[derive(Clone, Copy, CStruct)]
pub struct SortIter {
    pub b:    *mut BtreeNode,
    pub used: c_uint,
    pub size: c_uint,

    pub data: [SortIterSet; 0],
}
c_default!(SortIter);

#[repr(C)]
#[c_name(sort_iter_stack)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct SortIterStack {
    pub iter: SortIter,
    #[c("struct sort_iter_set sets[MAX_BSETS + 1]")]
    pub sets: [SortIterSet; MAX_BSETS as usize + 1],
}

c_enum! {
    #[closed]
    #[c_name(compact_mode)]
    pub enum CompactMode: u32 {
        COMPACT_LAZY,
        COMPACT_ALL,
    }
}

const SECTOR_U64S: usize = 512 / size_of::<u64>();
const NODE_U64S: usize = size_of::<Head<c::btree_node>>() / size_of::<u64>();
const ENTRY_U64S: usize = size_of::<Head<c::btree_node_entry>>() / size_of::<u64>();
const ENTRY_KEYS: usize = offset_of!(Head<c::btree_node_entry>, keys) / size_of::<u64>();

fn bset_written(b: &BtreeNode, t: &BsetTree) -> bool {
    (t.data_offset as usize) < b.written.get() as usize * SECTOR_U64S
}

/// Runs of keys, merged: by position, then by @tie_break - of each key's
/// offset and key.
struct Merge<'a, T> {
    keys:      &'a Keys<'a>,
    /// The runs not yet at their ends.
    sets:      &'a mut [SortIterSet],
    tie_break: T,
}

impl<'a, T: Fn(usize, &BkeyP, usize, &BkeyP) -> Ordering> Merge<'a, T> {
    #[inline]
    fn new(keys: &'a Keys<'a>, sets: &'a mut [SortIterSet], tie_break: T) -> Self {
        let mut iter = Merge { keys, sets, tie_break };

        iter.sort();
        iter
    }

    #[inline]
    fn key_at(&self, at: usize) -> &'a BkeyP {
        key_at(self.keys.buf, at)
    }

    #[inline(always)]
    fn key_cmp(&self, l: usize, r: usize) -> Ordering {
        let (lk, rk) = (self.key_at(l), self.key_at(r));

        self.keys.node().cmp_packed_inlined(lk, rk).then((self.tie_break)(l, lk, r, rk))
    }

    #[inline(always)]
    fn sift(&mut self, from: usize) {
        let mut i = from;

        while i + 1 < self.sets.len() &&
              self.key_cmp(self.sets[i].k as usize, self.sets[i + 1].k as usize).is_gt() {
            self.sets.swap(i, i + 1);
            i += 1;
        }
    }

    #[inline]
    fn sort(&mut self) {
        for i in (0..self.sets.len()).rev() {
            self.sift(i);
        }
    }

    #[inline]
    fn peek(&self) -> Option<usize> {
        self.sets.first().map(|s| s.k as usize)
    }

    #[inline(always)]
    fn advance(&mut self) {
        let s = &mut self.sets[0];
        let k = s.k as usize + key_at(self.keys.buf, s.k as usize).u64s();

        invariant!(k <= s.end as usize);
        s.k = k as u32;

        if s.k == s.end {
            let sets = core::mem::take(&mut self.sets);
            sets.copy_within(1.., 0);
            let n = sets.len() - 1;
            self.sets = &mut sets[..n];
        } else {
            self.sift(0);
        }
    }

    /*
     * key_sort_cmp() ensures that when keys compare equal the older key
     * comes first; so if l->k compares equal to r->k then l->k is older
     * and should be dropped.
     */
    #[inline]
    fn should_drop_next_key(&self) -> bool {
        match &*self.sets {
            [l, r, ..] => self.keys.node().cmp_packed_inlined(self.key_at(l.k as usize),
                                                              self.key_at(r.k as usize)).is_eq(),
            _ => false,
        }
    }

    /// The keys in order that @keep keeps, but those should_drop_next_key()
    /// drops: @keep first, the cheaper test.
    #[inline]
    fn unique(mut self, keep: impl Fn(usize, &BkeyP) -> bool) -> impl Iterator<Item = (usize, &'a BkeyP)> {
        core::iter::from_fn(move || loop {
            let at = self.peek()?;
            let k = self.key_at(at);
            let ret = keep(at, k) && !self.should_drop_next_key();

            self.advance();
            if ret {
                return Some((at, k));
            }
        })
    }
}

impl<'a, T: Fn(usize, &BkeyP, usize, &BkeyP) -> Ordering> Iterator for Merge<'a, T> {
    type Item = (usize, &'a BkeyP);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let at = self.peek()?;

        self.advance();
        Some((at, self.key_at(at)))
    }
}

/// The sorted keys fit their room: a sort only drops keys, and the room is as
/// big as its input.
#[inline(always)]
fn copy_key(dst: &mut [u64], out: usize, k: &BkeyP, f: impl FnOnce(&mut [u64])) -> usize {
    let src = k.as_slice();

    invariant!(out <= dst.len(), src.len() <= dst.len() - out);
    let dst = &mut dst[out..][..src.len()];

    unsafe { copy_u64s_small(dst.as_mut_ptr(), src.as_ptr(), src.len()) };
    f(dst);
    out + src.len()
}

fn set_needs_whiteout(k: &mut [u64], v: bool) {
    BkeyP::from_slice_mut(k).expect("a key just copied").set_needs_whiteout(v)
}

fn move_down(buf: &mut [u64], from: usize, len: usize, to: usize) {
    assert!(to <= from && from + len <= buf.len(),
            "moving {len} u64s from {from} to {to} of a {} u64 buffer", buf.len());

    let p = buf.as_mut_ptr();
    unsafe { copy_u64s_small(p.add(to), p.add(from), len) };
}

fn verify_no_dups(b: &BtreeNode, buf: &[u64], start: usize, end: usize) {
    if !cfg!(CONFIG_BCACHEFS_DEBUG) {
        return;
    }

    keys_in(buf, start, end).map(|(_, k)| k.unpack(b)).reduce(|l, r| {
        assert!(l.p < r.start_pos(), "unwritten whiteouts out of order, or overlapping: {l}, then {r}");
        r
    });
}

fn sort_bkey_ptrs(keys: &Keys<'_>, ptrs: &mut [u64]) {
    let cmp = |l: u64, r: u64| keys.node().cmp_packed(key_at(keys.buf, l as usize), key_at(keys.buf, r as usize));
    let (mut n, mut a) = (ptrs.len(), ptrs.len() / 2);

    if a == 0 {
        return;
    }

    /* Heap sort: see lib/sort.c: */
    loop {
        if a != 0 {
            a -= 1;
        } else {
            n -= 1;
            if n == 0 {
                break;
            }
            ptrs.swap(0, n);
        }

        let mut b = a;
        let (mut c, mut d);
        loop {
            c = 2 * b + 1;
            d = c + 1;
            if d >= n {
                break;
            }
            b = if cmp(ptrs[c], ptrs[d]).is_ge() { c } else { d };
        }
        if d == n {
            b = c;
        }

        while b != a && cmp(ptrs[a], ptrs[b]).is_ge() {
            b = (b - 1) / 2;
        }
        c = b;
        while b != a {
            b = (b - 1) / 2;
            ptrs.swap(b, c);
        }
    }
}

#[c_export]
pub fn bch2_sort_whiteouts(fs: &Fs, b: &mut BtreeNode) {
    let u64s = b.whiteout_u64s.get() as usize;

    if u64s == 0 {
        return;
    }

    /*
     * __bch2_bkey_unpack_key_b reads one byte before the key for its
     * header trick; pad the start of the bounce buffer so that read is
     * always against valid memory.
     */
    let mut bounce = Bounce::new(fs, (1 + u64s) * size_of::<u64>());
    let buf = bounce.u64s();
    let keys = b.keys();
    let start = keys.buf.len() - u64s;

    {
        /*
         * One offset per whiteout, in the last u64s of the sorted keys' room:
         * no key is smaller, so copying in the nth key leaves the offsets
         * after it unwritten
         */
        let new = &mut buf[1..];
        let mut ptrs = u64s;

        for (k, _) in keys_in(keys.buf, start, keys.buf.len()) {
            ptrs -= 1;
            new[ptrs] = k as u64;
        }

        sort_bkey_ptrs(&keys, &mut new[ptrs..]);

        let mut out = 0;
        for i in ptrs..u64s {
            let at = new[i] as usize;
            out = copy_key(new, out, key_at(keys.buf, at), |_| {});
        }
    }

    verify_no_dups(keys.node(), buf, 1, 1 + u64s);

    b.keys_mut().buf[start..].copy_from_slice(&buf[1..]);
}

fn should_compact_bset_lazy(nr: &BtreeNrKeys, set: usize, t: &BsetTree) -> bool {
    let total_u64s = t.u64s();
    let dead_u64s = total_u64s - nr.bset_u64s[set] as usize;

    dead_u64s > 64 && dead_u64s * 3 > total_u64s
}

fn should_compact_bset(b: &BtreeNode, set: usize, compacting: bool, mode: CompactMode) -> bool {
    let t = &b.set[set];

    if t.u64s() == b.nr.bset_u64s[set] as usize {
        return false;
    }

    match mode {
        CompactMode::COMPACT_LAZY => should_compact_bset_lazy(&b.nr, set, t) ||
            (compacting && !bset_written(b, t)),
        CompactMode::COMPACT_ALL  => true,
    }
}

#[c_export]
pub fn bch2_drop_whiteouts(b: &mut BtreeNode, mode: CompactMode) -> bool {
    let write_block = b.written.get() as usize * SECTOR_U64S;
    let mut ret = false;

    for set in 0..b.nsets as usize {
        let t = b.set[set];
        let moved = (set != 0 && !bset_written(b, &t))
            .then(|| (t.data_offset as usize - ENTRY_KEYS, write_block.max(b.set[set - 1].end_offset as usize)))
            .filter(|(src, dst)| src != dst);

        ret |= moved.is_some();

        let compact = should_compact_bset(b, set, ret, mode);
        let mut keys = b.keys_mut();

        if let Some((src, dst)) = moved {
            let u64s = ENTRY_U64S + if compact { 0 } else { t.u64s() };

            keys.buf.copy_within(src..src + u64s, dst);
            keys.set_bset(set, (dst + ENTRY_KEYS) as u16);
        }

        if !compact {
            continue;
        }

        let (mut k, end) = (t.first_offset() as usize, t.end_offset as usize);
        let first = keys.sets[set].first_offset() as usize;
        let mut out = first;

        while k != end {
            let key = key_at(keys.buf, k);
            let u64s = key.u64s();

            if !key.key_type().is_deleted() {
                move_down(keys.buf, k, u64s, out);
                out += u64s;
            } else {
                assert!(!key.needs_whiteout(), "dropping a whiteout at {k} that needs writing");
            }
            k += u64s;
        }

        let data_offset = keys.sets[set].data_offset;
        keys.bset_head_mut(data_offset).set_u64s(out - first);
        keys.set_bset_end(set);
        keys.sets.set_no_aux_tree(set);
        ret = true;
    }

    b.keys_mut().verify_btree_nr_keys();

    bch2_btree_build_aux_trees(b);

    ret
}

fn btree_node_sort(fs: &Fs, b: &mut BtreeNode, start_idx: usize, end_idx: usize) {
    let shift = end_idx - start_idx - 1;
    let sorting_entire_node = start_idx == 0 && end_idx == b.nsets as usize;

    let keys = b.keys();
    let sets = &keys.sets[start_idx..end_idx];
    let mut runs: ArrayVec<SortIterSet, { MAX_BSETS as usize }> = sets.iter()
        .filter(|t| t.u64s() != 0)
        .map(|t| SortIterSet { k: t.first_offset().into(), end: t.end_offset.into() })
        .collect();

    let bytes = if sorting_entire_node {
        b.buf_bytes()
    } else {
        (NODE_U64S + sets.iter().map(BsetTree::u64s).sum::<usize>()) * size_of::<u64>()
    };

    let mut out = Bounce::new(fs, bytes);

    let start_time = local_clock();

    /*
     * Main sort routine for compacting a btree node in memory: we always drop
     * whiteouts because any whiteouts that need to be written are in the unwritten
     * whiteouts area:
     */
    let room = &mut out.u64s()[NODE_U64S..];
    let u64s = Merge::new(&keys, &mut runs, |_, _, _, _| Ordering::Equal)
        .filter(|(_, k)| !k.key_type().is_deleted())
        .fold(0, |out, (_, k)| copy_key(room, out, k, |_| {}));

    if sorting_entire_node {
        fs.times[c::bch_time_stats::BCH_TIME_btree_node_sort as usize].update(start_time);
    }

    /* Make sure we preserve bset journal_seq: */
    let seq = sets.iter().map(|t| keys.bset(t).journal_seq.get()).max().unwrap_or(0);
    let (start_offset, first) = (sets[0].data_offset, sets[0].first_offset() as usize);
    b.keys_mut().bset_head_mut(start_offset).journal_seq = Le64::new(seq);

    if sorting_entire_node &&
       !mem_alloc_profiling_enabled() {
        /*
         * Our temporary buffer is the same size as the btree node's
         * buffer, we can just swap buffers instead of doing a big
         * memcpy()
         */
        out.u64s()[..NODE_U64S].copy_from_slice(&b.keys().buf[..NODE_U64S]);
        out.swap(b);
    } else {
        b.keys_mut().buf[first..][..u64s].copy_from_slice(&out.u64s()[NODE_U64S..][..u64s]);
    }

    let nsets = b.nsets as usize;
    b.nr.bset_u64s[start_idx] += b.nr.bset_u64s[start_idx + 1..end_idx].iter().sum::<u16>();
    b.nr.bset_u64s.copy_within(end_idx..nsets, start_idx + 1);
    b.set.copy_within(end_idx..nsets, start_idx + 1);
    b.nsets -= shift as u8;
    b.nr.bset_u64s[b.nsets as usize..].fill(0);

    let mut keys = b.keys_mut();
    keys.bset_head_mut(start_offset).set_u64s(u64s);
    keys.set_bset_end(start_idx);
    keys.sets.set_no_aux_tree(start_idx);

    drop(out);

    keys.verify_btree_nr_keys();
}

#[c_export]
pub fn bch2_btree_sort_into(fs: &Fs, dst: &mut BtreeNode, src: &BtreeNode) {
    let start_time = local_clock();

    assert!(dst.nsets == 1, "sorting into a node with {} bsets", dst.nsets);

    let mut dst = dst.keys_mut();
    dst.sets.set_no_aux_tree(0);

    let src_keys = src.keys();
    let mut src_iter = KeyIter::default();
    src_iter.init_from_start(&src_keys);

    /* Sort + repack in a new format: */
    let (in_f, out_f) = (&src.format, dst.format);
    let transform = plain_bytes(out_f) != plain_bytes(in_f);
    let t = dst.sets[0];
    let mut out = t.end_offset as usize;

    while let Some(i) = src_iter.peek(&src_keys) {
        let k = src_keys.key_at(i);

        src_iter.advance(&src_keys);

        if !transform {
            copy_key(dst.buf, out, k, |_| {});
        } else {
            let room = &mut dst.buf[out..];
            assert!(room.len() >= BKEY_U64S + k.val_u64s(in_f),
                    "repacking a {} u64 key into a node with {} u64s left", k.u64s(), room.len());

            let k_f = if k.is_packed() { in_f } else { unsafe { &*&raw const c::bch2_bkey_format_current } };
            if unsafe { c::bch2_bkey_transform(out_f, room.as_mut_ptr().cast(), k_f, k.as_ffi()) } {
                BkeyP::from_slice_mut(room).expect("a transformed key").set_format(c::KEY_FORMAT_LOCAL_BTREE as u8);
            } else {
                unsafe { c::bch2_bkey_unpack(src, room.as_mut_ptr().cast(), k.as_ffi()) }
            }
        }

        let k = BkeyP::from_slice_mut(&mut dst.buf[out..]).expect("a repacked key");
        k.set_needs_whiteout(false);

        let k = &*k;
        dst.nr.account_key_add(0, k.cast());
        out += k.u64s();
    }

    dst.bset_head_mut(t.data_offset).set_u64s(out - t.first_offset() as usize);

    fs.times[c::bch_time_stats::BCH_TIME_btree_node_sort as usize].update(start_time);

    dst.set_bset_end(0);
    dst.verify_btree_nr_keys();
}

/*
 * We're about to add another bset to the btree node, so if there's currently
 * too many bsets - sort some of them together:
 */
#[c_export]
pub fn bch2_btree_node_compact(fs: &Fs, b: &mut BtreeNode) -> bool {
    let nsets = b.nsets as usize;
    let unwritten_idx = b.sets().iter().position(|t| !bset_written(b, t)).unwrap_or(nsets);
    let mut ret = false;

    for (start, end) in [(unwritten_idx, nsets), (0, unwritten_idx)] {
        if end - start > 1 {
            btree_node_sort(fs, b, start, end);
            ret = true;
        }
    }
    ret
}

#[c_export]
pub fn bch2_btree_build_aux_trees(b: &mut BtreeNode) {
    let nsets = b.nsets as usize;
    let written: [bool; MAX_BSETS as usize] = core::array::from_fn(|i| i < nsets && bset_written(b, &b.set[i]));
    let mut keys = b.keys_mut();

    for set in 0..nsets {
        keys.bset_build_aux_tree(set, !written[set] && set + 1 == nsets);
    }
}

/* For C: pointers into the node in, offsets inside */

/// # Safety
/// @iter is sort_iter_init()'s, its runs sort_iter_add()'s.
unsafe fn sort_iter<'a>(iter: *mut SortIter) -> (&'a BtreeNode, &'a mut [SortIterSet]) {
    unsafe {
        let used = (*iter).used as usize;
        (&*(*iter).b, core::slice::from_raw_parts_mut((&raw mut (*iter).data).cast(), used))
    }
}

fn runs_u64s(runs: &[SortIterSet]) -> usize {
    runs.iter().map(|s| (s.end - s.k) as usize).sum()
}

/// Of a pointer C has into @keys' buffer, or to its end.
fn buf_offset(keys: &Keys<'_>, p: *const c::bkey_packed) -> u32 {
    let offset = (p as usize).wrapping_sub(keys.buf.as_ptr() as usize) / size_of::<u64>();

    assert!(offset <= keys.buf.len(), "{p:p}, not in the node buffer at {:p} of {} u64s",
            keys.buf.as_ptr(), keys.buf.len());
    offset as u32
}

/// Keys [@k, @end) of @iter's node, in order, to merge.
///
/// # Safety
/// @iter is sort_iter_init()'s.
#[c_export]
pub unsafe fn sort_iter_add(iter: *mut SortIter, k: *const c::bkey_packed, end: *const c::bkey_packed) {
    unsafe {
        let (used, size) = ((*iter).used, (*iter).size);
        assert!(used < size, "adding to a sort iterator of {size} runs, full");

        let keys = (*(*iter).b).keys();
        let (k, end) = (buf_offset(&keys, k), buf_offset(&keys, end));

        if k != end {
            (&raw mut (*iter).data).cast::<SortIterSet>().add(used as usize).write(SortIterSet { k, end });
            (*iter).used += 1;
        }
    }
}

/// Sets each key's needs_whiteout: they're all on disk.
///
/// # Safety
/// @iter's runs are of its node's bsets, and @dst has room for their keys.
#[c_export]
pub unsafe fn bch2_key_sort_fix_overlapping(_fs: &Fs, dst: *mut Head<c::bset>, iter: *mut SortIter) -> BtreeNrKeys {
    let (b, runs) = unsafe { sort_iter(iter) };
    let room = unsafe { core::slice::from_raw_parts_mut((&raw mut (*dst)._data).cast(), runs_u64s(runs)) };
    let mut nr = BtreeNrKeys::default();

    /*
     * If keys compare equal, compare by pointer order:
     */
    let u64s = Merge::new(&b.keys(), runs, |l, _, r, _| l.cmp(&r))
        .unique(|_, k| !k.key_type().is_deleted())
        .fold(0, |out, (_, k)| {
            nr.account_key_add(0, k);
            copy_key(room, out, k, |k| set_needs_whiteout(k, true))
        });

    unsafe { (*dst).set_u64s(u64s) };
    nr
}

/// Clears each key's needs_whiteout: it isn't written.
///
/// # Safety
/// @iter's runs are of its node, and @dst has room for their keys.
#[c_export]
pub unsafe fn bch2_sort_keys_keep_unwritten_whiteouts(dst: *mut c::bkey_packed, iter: *mut SortIter) -> c_uint {
    let (b, runs) = unsafe { sort_iter(iter) };
    let room = unsafe { core::slice::from_raw_parts_mut(dst.cast(), runs_u64s(runs)) };
    let keys = b.keys();
    let unwritten_whiteouts = keys.buf.len() - b.whiteout_u64s.get() as usize;

    /*
     * For sorting in the btree node write path: whiteouts not in the unwritten
     * whiteouts area are dropped, whiteouts in the unwritten whiteouts area are
     * dropped if overwritten by real keys:
     */
    Merge::new(&keys, runs, |l, lk, r, rk| rk.key_type().is_deleted().cmp(&lk.key_type().is_deleted())
                                               .then(l.cmp(&r)))
        .unique(|at, k| !(k.key_type().is_deleted() && at < unwritten_whiteouts))
        .fold(0, |out, (_, k)| copy_key(room, out, k, |k| set_needs_whiteout(k, false))) as c_uint
}

#[c_export]
pub fn bch2_btree_node_set_needs_whiteout(b: &mut BtreeNode) {
    b.keys_mut().iter_mut().for_each(|k| k.set_needs_whiteout(true));
}

#[c_export]
pub fn bch2_maybe_compact_whiteouts(_fs: &Fs, b: &mut BtreeNode) -> bool {
    let compact = b.sets().iter().enumerate().any(|(set, t)| should_compact_bset_lazy(&b.nr, set, t));

    compact && bch2_drop_whiteouts(b, CompactMode::COMPACT_LAZY)
}

#[c_export]
pub fn bch2_btree_node_sort(fs: &Fs, b: &mut BtreeNode, start_idx: c_uint, end_idx: c_uint) {
    btree_node_sort(fs, b, start_idx as usize, end_idx as usize)
}
