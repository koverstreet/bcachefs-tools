// SPDX-License-Identifier: GPL-2.0

/* Btree node iterator */

use super::bset::BsetTree;
use super::keys::Keys;
use super::{check_lookups, BtreeNode, NodeContents};
use crate::btree::bkey::{BkeyP, BkeySC, BkeyV};
use crate::c;
use crate::cstructs::c as cs;
use crate::format::Bpos;
use crate::fs::Fs;
use crate::prt;
use crate::debug::trace::events;
use crate::util::kernel::unlikely;
use crate::util::log::{print_lines, LogLevel};
use crate::util::Printbuf;
use core::cmp::Ordering;
use cstruct_macros::CStruct;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct KeyIterSet {
    pub k: u16,
    pub end: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct KeyIter {
    pub data: [KeyIterSet; cs::MAX_BSETS as usize],
}

const NR: usize = cs::MAX_BSETS as usize;

#[inline(always)]
fn prefetch_four_cachelines(p: *const u64) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("prefetcht0 [{p} - 127 + 64 * 0]",
                         "prefetcht0 [{p} - 127 + 64 * 1]",
                         "prefetcht0 [{p} - 127 + 64 * 2]",
                         "prefetcht0 [{p} - 127 + 64 * 3]",
                         p = in(reg) p.cast::<u8>().wrapping_add(127),
                         options(nostack, readonly, preserves_flags))
    }
    #[cfg(not(target_arch = "x86_64"))]
    for i in 0..4 {
        crate::util::kernel::prefetch(p.cast::<u8>().wrapping_add(crate::types::L1_CACHE_BYTES * i));
    }
}

impl Keys<'_> {
    #[inline(always)]
    fn btree_node_iter_cmp(&self, l: KeyIterSet, r: KeyIterSet) -> Ordering {
        self.contents().bkey_iter_cmp(self.key_at(l.k), self.key_at(r.k))
    }

    fn bset_tree_find(&self, end_offset: u16) -> Option<&BsetTree> {
        self.sets.iter().find(|t| t.end_offset == end_offset)
    }
}

impl KeyIter {
    pub fn __set_end(&self, i: usize) -> bool {
        self.data[i].k == self.data[i].end
    }

    pub fn end(&self) -> bool {
        self.__set_end(0)
    }

    /// btree_node_iter_for_each(): the sets it's at, from the first.
    pub fn sets(&self) -> impl Iterator<Item = &KeyIterSet> {
        self.data.iter().take_while(|s| s.k != s.end)
    }

    pub fn set_find(&self, end_offset: u16) -> Option<usize> {
        self.sets().position(|s| s.end == end_offset)
    }

    fn __used(&self) -> usize {
        NR - self.data.iter().rev().take_while(|s| s.k == s.end).count()
    }

    #[cold]
    #[inline(never)]
    fn bad(&self, keys: Option<&Keys<'_>>, msg: &str) -> ! {
        let mut buf = Printbuf::new();

        prt!(&mut buf, "btree node iterator: {msg}\niter (k end):");
        for s in &self.data {
            prt!(&mut buf, " [{} {}]", s.k, s.end);
        }

        if let Some(keys) = keys {
            prt!(&mut buf, "\nbsets (start end):");
            for t in keys.sets {
                prt!(&mut buf, " [{} {}]", t.first_offset(), t.end_offset);
            }
        }

        panic!("{}\n", buf.as_str());
    }

    /*
     * Live entries are contiguous from data[0], and each tracks a different bset.
     * __btree_node_iter_used() counts from the end and btree_node_iter_for_each()
     * from the start, so a hole or a duplicate makes them disagree about where the
     * next push goes - an overflow of data[] (GH #935).
     *
     * Unlike the rest of __bch2_btree_node_iter_verify() this holds between every
     * step of an update, so the helpers below check it mid-update too.
     */
    fn verify_shape(&self, keys: Option<&Keys<'_>>) {
        if !cfg!(CONFIG_BCACHEFS_DEBUG) {
            return;
        }

        let live = &self.data[..self.__used()];

        if live.iter().any(|s| s.k == s.end) {
            self.bad(keys, "empty entry before a live one");
        }

        if (1..live.len()).any(|i| live[..i].iter().any(|s| s.end == live[i].end)) {
            self.bad(keys, "two entries track the same bset");
        }
    }

    fn __push(&mut self, keys: &Keys<'_>, k: u16, end: u16) {
        if k != end {
            let pos = self.__used();

            if cfg!(CONFIG_BCACHEFS_DEBUG) {
                self.verify_shape(Some(keys));

                if self.data[..pos].iter().any(|i| i.end == end) {
                    self.bad(Some(keys), "push of a bset already tracked");
                }

                if pos >= NR {
                    self.bad(Some(keys), "push into a full iterator");
                }
            }

            self.data[pos] = KeyIterSet { k, end };
        }
    }

    pub fn push(&mut self, keys: &Keys<'_>, k: u16, end: u16) {
        self.__push(keys, k, end);
        self.sort(keys);
    }

    #[cold]
    #[inline(never)]
    fn init_pack_failed(&mut self, fs: &Fs, keys: &Keys<'_>, search: &Bpos) {
        let b = keys.node();

        fs.event_inc_trace(events::bkey_pack_pos_fail, |out| {
            prt!(out, "{}", b.to_text(fs));
            prt!(out, "failed to pack {search}");
        });

        self.init_from_start(keys);

        while let Some(k) = self.peek(keys) {
            if !keys.bkey_iter_pos_cmp(k, search).is_lt() {
                break;
            }
            self.advance(keys);
        }
    }

    /**
     * bch2_btree_node_iter_init - initialize a btree node iterator, starting from a
     * given position
     *
     * @c:		filesystem object
     * @b:		btree node to search
     * @iter:	iterator to initialize
     * @search:	search key
     *
     * Main entry point to the lookup code for individual btree nodes:
     *
     * NOTE:
     *
     * When you don't filter out deleted keys, btree nodes _do_ contain duplicate
     * keys. This doesn't matter for most code, but it does matter for lookups.
     *
     * Some adjacent keys with a string of equal keys:
     *	i j k k k k l m
     *
     * If you search for k, the lookup code isn't guaranteed to return you any
     * specific k. The lookup code is conceptually doing a binary search and
     * iterating backwards is very expensive so if the pivot happens to land at the
     * last k that's what you'll get.
     *
     * This works out ok, but it's something to be aware of:
     *
     *  - For non extents, we guarantee that the live key comes last - see
     *    btree_node_iter_cmp(), keys_out_of_order(). So the duplicates you don't
     *    see will only be deleted keys you don't care about.
     *
     *  - For extents, deleted keys sort last (see the comment at the top of this
     *    file). But when you're searching for extents, you actually want the first
     *    key strictly greater than your search key - an extent that compares equal
     *    to the search key is going to have 0 sectors after the search key.
     *
     *    But this does mean that we can't just search for
     *    bpos_successor(start_of_range) to get the first extent that overlaps with
     *    the range we want - if we're unlucky and there's an extent that ends
     *    exactly where we searched, then there could be a deleted key at the same
     *    position and we'd get that when we search instead of the preceding extent
     *    we needed.
     *
     *    So we've got to search for start_of_range, then after the lookup iterate
     *    past any extents that compare equal to the position we searched for.
     */
    pub fn init(&mut self, fs: &Fs, keys: &Keys<'_>, search: &Bpos) {
        let b = keys.contents();
        let mut p = c::bkey_packed_padded::default();
        let mut k = [0u16; NR];

        ebug_assert!(*search >= keys.min_key());
        ebug_assert!(*search <= keys.max_key());
        keys.bset_aux_tree_verify();

        *self = Self::default();

        let (buf, at) = p.as_buf_mut();
        let exact = match b.pack_pos_lossy(buf, at, *search) {
            c::BkeyPackPosRet::BKEY_PACK_POS_EXACT   => true,
            c::BkeyPackPosRet::BKEY_PACK_POS_SMALLER => false,
            c::BkeyPackPosRet::BKEY_PACK_POS_FAIL    => {
                self.init_pack_failed(fs, keys, search);
                return;
            }
        };
        let lossy_packed_search: &BkeyP = BkeyP::from_packed(&p.k).expect("a packed search key").cast();
        let packed_search = exact.then_some(lossy_packed_search);

        for (k, t) in k.iter_mut().zip(keys.sets) {
            *k = keys.__bset_search(t, search, &p.k);
            prefetch_four_cachelines(keys.buf.as_ptr().wrapping_add(*k as usize));
        }

        let mut pos = 0;
        for (k, t) in k.iter_mut().zip(keys.sets) {
            let end = t.end_offset;

            *k = keys.bset_search_linear(t, search, packed_search, lossy_packed_search, *k);
            if *k != end {
                invariant!(pos < NR);
                self.data[pos] = KeyIterSet { k: *k, end };
                pos += 1;
            }
        }

        self.sort(keys);
    }

    pub fn init_from_start(&mut self, keys: &Keys<'_>) {
        *self = Self::default();

        for t in keys.sets {
            self.__push(keys, t.first_offset(), t.end_offset);
        }
        self.sort(keys);
    }

    pub fn bset_pos(&self, t: &BsetTree) -> u16 {
        self.set_find(t.end_offset).map_or(t.end_offset, |i| self.data[i].k)
    }

    #[inline(always)]
    fn sort_two(&mut self, keys: &Keys<'_>, first: usize) -> bool {
        let ret = keys.btree_node_iter_cmp(self.data[first], self.data[first + 1]).is_gt();

        if ret {
            self.data.swap(first, first + 1);
        }
        ret
    }

    pub fn sort(&mut self, keys: &Keys<'_>) {
        /* unrolled bubble sort: */

        if !self.__set_end(2) {
            self.sort_two(keys, 0);
            self.sort_two(keys, 1);
        }

        if !self.__set_end(1) {
            self.sort_two(keys, 0);
        }

        self.verify_shape(Some(keys));
    }

    pub fn set_drop(&mut self, set: usize) {
        self.data.copy_within(set + 1.., set);
        self.data[NR - 1] = KeyIterSet::default();

        self.verify_shape(None);
    }

    pub fn __peek_all(&self) -> u16 {
        self.data[0].k
    }

    pub fn peek_all(&self) -> Option<u16> {
        (!self.end()).then_some(self.data[0].k)
    }

    #[inline(always)]
    pub fn peek(&mut self, keys: &Keys<'_>) -> Option<u16> {
        while let Some(k) = self.peek_all() {
            if !keys.key_at(k).key_type().is_deleted() {
                return Some(k);
            }
            self.advance(keys);
        }
        None
    }

    #[inline(always)]
    fn __advance(&mut self, keys: &Keys<'_>) {
        self.data[0].k = keys.bkey_p_next(self.__peek_all());

        ebug_assert!(self.data[0].k <= self.data[0].end);

        if unlikely(self.__set_end(0)) {
            /* avoid an expensive memmove call: */
            self.data[0] = self.data[1];
            self.data[1] = self.data[2];
            self.data[2] = KeyIterSet::default();
            return;
        }

        if self.__set_end(1) {
            return;
        }

        if !self.sort_two(keys, 0) {
            return;
        }

        if self.__set_end(2) {
            return;
        }

        self.sort_two(keys, 1);
    }

    pub fn advance(&mut self, keys: &Keys<'_>) {
        if check_lookups() {
            self.__verify(keys);
            self.__next_check(keys);
        }

        self.__advance(keys);
    }

    /*
     * Expensive:
     */
    pub fn prev_all(&mut self, keys: &Keys<'_>) -> Option<u16> {
        let b = keys.contents();
        let mut prev: Option<u16> = None;
        let mut end = 0;

        self.verify(keys);

        for t in keys.sets {
            if let Some(k) = keys.bkey_prev_all(t, self.bset_pos(t)) {
                if prev.is_none_or(|p| b.bkey_iter_cmp(keys.key_at(k), keys.key_at(p)).is_gt()) {
                    prev = Some(k);
                    end = t.end_offset;
                }
            }
        }

        let prev = prev?;

        /*
         * We're manually memmoving instead of just calling sort() to ensure the
         * prev we picked ends up in slot 0 - sort won't necessarily put it
         * there because of duplicate deleted keys:
         */
        let set = self.set_find(end).unwrap_or_else(|| self.__used());

        ebug_assert!(set < NR);

        self.data.copy_within(0..set, 1);
        self.data[0] = KeyIterSet { k: prev, end };

        self.verify(keys);
        Some(prev)
    }

    pub fn prev(&mut self, keys: &Keys<'_>) -> Option<u16> {
        loop {
            let prev = self.prev_all(keys);
            if prev.is_none_or(|k| !keys.key_at(k).key_type().is_deleted()) {
                return prev;
            }
        }
    }

    pub fn peek_unpack<'u>(&mut self, keys: &Keys<'u>, u: &'u mut c::bkey) -> Option<BkeySC<'u>> {
        let k = self.peek(keys)?;

        Some(keys.disassemble(k, u))
    }

    fn set_set_pos(&mut self, keys: &Keys<'_>, end_offset: u16, k: u16) {
        for i in 0..NR {
            if self.__set_end(i) {
                break;
            }

            if self.data[i].end == end_offset {
                self.data[i].k = k;
                self.sort(keys);
                return;
            }
        }

        self.push(keys, k, end_offset);
    }

    pub fn fix_key_modified(&mut self, keys: &Keys<'_>, where_: u16, pos: &Bpos) {
        if self.peek_all() == Some(where_) && keys.bkey_iter_pos_cmp(where_, pos).is_lt() {
            self.advance(keys);
            return;
        }

        /*
         * The caller just flipped @where's deleted-ness in place (mark
         * deleted for an overwrite, or same-position whiteout). That's
         * the tiebreaker in bkey_iter_cmp(), so any iter set entry whose
         * bset_pos shares a key value with @where may now be out of order
         * — and the breakage isn't limited to data[0]. Re-sort to restore
         * the iter's sort invariant.
         */
        self.sort(keys);
    }

    pub fn fix(&mut self, keys: &Keys<'_>, level: u8, t: usize, where_: u16, clobber_u64s: u16, new_u64s: u16,
               pos: &Bpos) {
        let end = keys.sets[t].end_offset;
        let offset = where_;
        let shift = i32::from(new_u64s) - i32::from(clobber_u64s);
        let old_end = (i32::from(end) - shift) as u16;

        match self.set_find(old_end) {
            None => {
                /*
                 * Bset isn't tracked by the iter. If the iter is positioned
                 * at-or-before the new key, push the bset in. Either way, fall
                 * through to the cross-bset rewind walk below: an insert into
                 * an untracked bset can still violate the cross-bset rewind
                 * invariant against a tracked bset (e.g. same bpos as a
                 * whiteout in another bset that the iter is currently parked
                 * on), and the rewind is what catches that.
                 */
                if new_u64s != 0 && keys.bkey_iter_pos_cmp(where_, pos).is_ge() {
                    self.push(keys, where_, end);
                }
            }
            Some(i) => {
                self.data[i].end = end;

                /*
                 * If the iter has reached or passed the change in this bset,
                 * adjust set->k. If it hasn't (set->k < offset), leave set->k
                 * alone — but still fall through to the rewind walk below in
                 * case the change introduced a cross-bset inconsistency
                 * against another bset this iter tracks.
                 */
                if self.data[i].k >= offset {
                    if new_u64s != 0 && keys.bkey_iter_pos_cmp(where_, pos).is_ge() {
                        self.data[i].k = offset;
                    } else if self.data[i].k < offset + clobber_u64s {
                        self.data[i].k = offset + new_u64s;
                        if self.data[i].k == self.data[i].end {
                            self.set_drop(i);
                        }
                    } else {
                        /* Iterator is after key that changed */
                        self.data[i].k = (i32::from(self.data[i].k) + shift) as u16;
                    }

                    self.sort(keys);
                }
            }
        }

        /*
         * Inserts/removes in interior nodes can leave a btree_node_iter
         * inconsistent: the modified key may now sit between the iter's
         * current key and a passed key in another bset. Multiple paths
         * can share an interior node, so the path doing the modification
         * isn't the only iter that needs adjusting. Always rewind to catch
         * passed keys (in any bset) that should now come after the iter's
         * current key — otherwise bch2_btree_node_iter_prev_all() breaks.
         */
        if !self.end() && level != 0 {
            let b = keys.contents();
            let k = keys.key_at(self.__peek_all());

            for t in keys.sets {
                if self.data[0].end == t.end_offset {
                    continue;
                }

                let mut k2 = self.bset_pos(t);
                let mut set_pos = false;

                loop {
                    let Some(p) = keys.bkey_prev_all(t, k2) else { break };
                    if b.bkey_iter_cmp(k, keys.key_at(p)).is_ge() {
                        break;
                    }
                    k2 = p;
                    set_pos = true;
                }

                if set_pos {
                    self.set_set_pos(keys, t.end_offset, k2);
                }
            }
        }

        self.verify(keys);
    }

    fn __next_check(&self, keys: &Keys<'_>) {
        let b = keys.contents();
        let mut iter = *self;

        let k = iter.__peek_all();
        iter.__advance(keys);
        let n = iter.peek_all();

        let _ = keys.key_at(k).unpack(b);

        let Some(n) = n else { return };
        let (k, n) = (keys.key_at(k), keys.key_at(n));

        if b.bkey_iter_cmp(k, n).is_gt() {
            let mut buf = Printbuf::new();

            keys.btree_node_bsets_to_text(&mut buf, None);
            prt!(&mut buf, "out of order/overlapping:\n");
            prt!(&mut buf, "{}\n", k.unpack(b));
            prt!(&mut buf, "{}\n", n.unpack(b));
            prt!(&mut buf, "iter was:");

            for set in self.sets() {
                let t = keys.bkey_to_bset(set.k);
                prt!(&mut buf, " [{t} {}]", set.k - keys.sets[t].first_offset());
            }

            print_lines(&LogLevel::ERR.prefix(), buf.as_bytes());
            panic!("__bch2_btree_node_iter_next_check");
        }
    }

    pub fn __verify(&self, keys: &Keys<'_>) {
        let b = keys.contents();

        /*
         * Before the early return, and before anything that iterates with
         * btree_node_iter_for_each(): a hole ends that walk early, hiding
         * itself and everything behind it.
         */
        self.verify_shape(Some(keys));

        if !check_lookups() || self.end() {
            return;
        }

        /* Verify that set->end is correct: */
        for (i, set) in self.sets().enumerate() {
            let t = keys.bset_tree_find(set.end);

            assert!(t.is_some_and(|t| set.k >= t.first_offset() && set.k < t.end_offset),
                    "btree node iterator entry {i} at [{} {}], not within a bset", set.k, set.end);
        }

        /* Verify iterator is sorted: */
        for i in 1..self.sets().count() {
            assert!(!keys.btree_node_iter_cmp(self.data[i - 1], self.data[i]).is_gt(),
                    "btree node iterator entries {} and {i} out of order", i - 1);
        }

        let k = keys.key_at(self.__peek_all());

        for t in keys.sets {
            if self.data[0].end == t.end_offset {
                continue;
            }

            if let Some(p) = keys.bkey_prev_all(t, self.bset_pos(t)) {
                assert!(!b.bkey_iter_cmp(k, keys.key_at(p)).is_lt(),
                        "btree node iterator at {}, past a key at {p} of the bset ending at {}",
                        self.data[0].k, t.end_offset);
            }
        }
    }

    /*
     * Always runs the cheap structural checks in debug builds; the expensive ones
     * are behind debug_check_bset_lookups.
     */
    pub fn verify(&self, keys: &Keys<'_>) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            self.__verify(keys);
        }
    }

    #[cold]
    pub fn dump(&self, keys: &Keys<'_>) {
        let b = keys.contents();
        let mut buf = Printbuf::new();

        prt!(&mut buf, "btree node iter with {}/{} sets:\n", self.__used(), keys.sets.len());

        for set in self.sets() {
            let t = keys.bkey_to_bset(set.k);
            prt!(&mut buf, "set {t} key {}: {}\n", set.k, keys.key_at(set.k).unpack(b));
        }

        print_lines(&LogLevel::ERR.prefix(), buf.as_bytes());
    }
}

impl BtreeNode {
    #[cold]
    pub fn btree_node_keys_to_text(&self, contents: &NodeContents, out: &mut Printbuf, fs: &Fs) {
        let _ = self.for_each_key(contents, |k| {
            prt!(out, "{}\n", k.to_text(fs));
            core::ops::ControlFlow::Continue(())
        });
    }

    /* For Rust, which doesn't see a packed key: that of the key @iter is at. */
    #[cold]
    pub fn btree_node_iter_bfloat_to_text(&self, contents: &NodeContents, out: &mut Printbuf, iter: &mut KeyIter) {
        if let Some(k) = iter.peek(&self.keys(contents)) {
            self.bfloat_to_text(contents, out, k);
        }
    }
}
