// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use super::aux_tree::{bset_search_tree, bset_search_write_set, rw_aux_tree_bsearch, AuxTree, BFLOAT_FAILED};
use super::keys::{key_at, Keys, KeysMut};
use super::{check_lookups, BtreeNode, BtreeNrKeys, NodeContents};
use crate::btree::bkey::{BkeyI, BkeyP, BkeyV, BKEY_U64S};
use crate::cstructs::c;
use crate::debug::params::debug_check_btree_accounting;
use crate::format::Bpos;
use crate::fs::Fs;
use crate::prt;
use crate::types::Le64;
use crate::util::kernel::{likely, random_u64};
use crate::util::log::{log_no_fs, print_lines, LogLevel};
use crate::util::printbuf::Msg;
use crate::util::vstructs::{cast_plain_mut, cast_plain_slice, cast_plain_slice_mut, plain_bytes, plain_bytes_mut, Head};
use crate::util::Printbuf;
use core::cmp::Ordering;
use core::fmt;
use core::mem::offset_of;
use cstruct_macros::{c_name, CStruct, bch_enum};

/*
 * BKEYS:
 *
 * A bkey contains a key, a size field, a variable number of pointers, and some
 * ancillary flag bits.
 *
 * We use two different functions for validating bkeys, bkey_invalid and
 * bkey_deleted().
 *
 * The one exception to the rule that ptr_invalid() filters out invalid keys is
 * that it also filters out keys of size 0 - these are keys that have been
 * completely overwritten. It'd be safe to delete these in memory while leaving
 * them on disk, just unnecessary work - so we filter them out when resorting
 * instead.
 *
 * We can't filter out stale keys when we're resorting, because garbage
 * collection needs to find them to ensure bucket gens don't wrap around -
 * unless we're rewriting the btree node those stale keys still exist on disk.
 *
 * We also implement functions here for removing some number of sectors from the
 * front or the back of a bkey - this is mainly used for fixing overlapping
 * extents, by removing the overlapping sectors from the older key.
 *
 * BSETS:
 *
 * A bset is an array of bkeys laid out contiguously in memory in sorted order,
 * along with a header. A btree node is made up of a number of these, written at
 * different times.
 *
 * There could be many of them on disk, but we never allow there to be more than
 * 4 in memory - we lazily resort as needed.
 *
 * We implement code here for creating and maintaining auxiliary search trees
 * (described below) for searching an individial bset, and on top of that we
 * implement a btree iterator.
 *
 * BTREE ITERATOR:
 *
 * Most of the code in bcache doesn't care about an individual bset - it needs
 * to search entire btree nodes and iterate over them in sorted order.
 *
 * The btree iterator code serves both functions; it iterates through the keys
 * in a btree node in sorted order, starting from either keys after a specific
 * point (if you pass it a search key) or the start of the btree node.
 *
 * AUXILIARY SEARCH TREES:
 *
 * Since keys are variable length, we can't use a binary search on a bset - we
 * wouldn't be able to find the start of the next key. But binary searches are
 * slow anyways, due to terrible cache behaviour; bcache originally used binary
 * searches and that code topped out at under 50k lookups/second.
 *
 * So we need to construct some sort of lookup table. Since we only insert keys
 * into the last (unwritten) set, most of the keys within a given btree node are
 * usually in sets that are mostly constant. We use two different types of
 * lookup tables to take advantage of this.
 *
 * Both lookup tables share in common that they don't index every key in the
 * set; they index one key every BSET_CACHELINE bytes, and then a linear search
 * is used for the rest.
 *
 * For sets that have been written to disk and are no longer being inserted
 * into, we construct a binary search tree in an array - traversing a binary
 * search tree in an array gives excellent locality of reference and is very
 * fast, since both children of any node are adjacent to each other in memory
 * (and their grandchildren, and great grandchildren...) - this means
 * prefetching can be used to great effect.
 *
 * It's quite useful performance wise to keep these nodes small - not just
 * because they're more likely to be in L2, but also because we can prefetch
 * more nodes on a single cacheline and thus prefetch more iterations in advance
 * when traversing this tree.
 *
 * Nodes in the auxiliary search tree must contain both a key to compare against
 * (we don't want to fetch the key from the set, that would defeat the purpose),
 * and a pointer to the key. We use a few tricks to compress both of these.
 *
 * To compress the pointer, we take advantage of the fact that one node in the
 * search tree corresponds to precisely BSET_CACHELINE bytes in the set. We have
 * a function (to_inorder()) that takes the index of a node in a binary tree and
 * returns what its index would be in an inorder traversal, so we only have to
 * store the low bits of the offset.
 *
 * The key is 84 bits (KEY_DEV + key->key, the offset on the device). To
 * compress that,  we take advantage of the fact that when we're traversing the
 * search tree at every iteration we know that both our search key and the key
 * we're looking for lie within some range - bounded by our previous
 * comparisons. (We special case the start of a search so that this is true even
 * at the root of the tree).
 *
 * So we know the key we're looking for is between a and b, and a and b don't
 * differ higher than bit 50, we don't need to check anything higher than bit
 * 50.
 *
 * We don't usually need the rest of the bits, either; we only need enough bits
 * to partition the key range we're currently checking.  Consider key n - the
 * key our auxiliary search tree node corresponds to, and key p, the key
 * immediately preceding n.  The lowest bit we need to store in the auxiliary
 * search tree is the highest bit that differs between n and p.
 *
 * Note that this could be bit 0 - we might sometimes need all 80 bits to do the
 * comparison. But we'd really like our nodes in the auxiliary search tree to be
 * of fixed size.
 *
 * The solution is to make them fixed size, and when we're constructing a node
 * check if p and n differed in the bits we needed them to. If they don't we
 * flag that node, and when doing lookups we fallback to comparing against the
 * real key. As long as this doesn't happen to often (and it seems to reliably
 * happen a bit less than 1% of the time), we win - even on failures, that key
 * is then more likely to be in cache than if we were doing binary searches all
 * the way, since we're touching so much less memory.
 *
 * The keys in the auxiliary search tree are stored in (software) floating
 * point, with an exponent and a mantissa. The exponent needs to be big enough
 * to address all the bits in the original key, but the number of bits in the
 * mantissa is somewhat arbitrary; more bits just gets us fewer failures.
 *
 * We need 7 bits for the exponent and 3 bits for the key's offset (since keys
 * are 8 byte aligned); using 22 bits for the mantissa means a node is 4 bytes.
 * We need one node per 128 bytes in the btree node, which means the auxiliary
 * search trees take up 3% as much memory as the btree itself.
 *
 * Constructing these auxiliary search trees is moderately expensive, and we
 * don't want to be constantly rebuilding the search tree for the last set
 * whenever we insert another key into it. For the unwritten set, we use a much
 * simpler lookup table - it's just a flat array, so index i in the lookup table
 * corresponds to the i range of BSET_CACHELINE bytes in the set. Indexing
 * within each byte range works the same as with the auxiliary search trees.
 *
 * These are much easier to keep up to date when we insert a key - we do it
 * somewhat lazily; when we shift a key up we usually just increment the pointer
 * to it, only when it would overflow do we go to the trouble of finding the
 * first key in that range of bytes again.
 */

pub const MAX_BSETS: u32 = 3;

#[repr(C)]
#[c_name(bset_tree)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct BsetTree {
    /*
     * We construct a binary tree in an array as if the array
     * started at 1, so that things line up on the same cachelines
     * better: see comments in aux_tree.rs at cacheline_to_bkey() for
     * details
     */

    /* size of the binary tree and prev array */
    pub size: u16,

    /* function of size - precalculated for to_inorder() */
    pub extra: u16,

    pub data_offset: u16,
    pub aux_data_offset: u16,
    pub end_offset: u16,
}

pub const BSET_NO_AUX_TREE_VAL: u16 = u16::MAX;
pub const BSET_RW_AUX_TREE_VAL: u16 = u16::MAX - 1;

/*
 * BSET_CACHELINE was originally intended to match the hardware cacheline size -
 * it used to be 64, but I realized the lookup code would touch slightly less
 * memory if it was 128.
 *
 * It definites the number of bytes (in struct bset) per struct bkey_float in
 * the auxiliar search tree - when we're done searching the bset_float tree we
 * have this many bytes left that we do a linear search over.
 *
 * Since (after level 5) every level of the bset_tree is on a new cacheline,
 * we're touching one fewer cacheline in the bset tree in exchange for one more
 * cacheline in the linear search - but the linear search might stop before it
 * gets to the second cacheline.
 */
pub const BSET_CACHELINE: usize = 256;

impl BsetTree {
    pub fn aux_tree_type(&self) -> AuxTreeType {
        let ty = match self.extra {
            BSET_NO_AUX_TREE_VAL => AuxTreeType::BSET_NO_AUX_TREE,
            BSET_RW_AUX_TREE_VAL => AuxTreeType::BSET_RW_AUX_TREE,
            _                    => AuxTreeType::BSET_RO_AUX_TREE,
        };
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!((self.size == 0) == (ty == AuxTreeType::BSET_NO_AUX_TREE),
                    "bset tree of size {} with aux tree {ty:?}", self.size);
        }
        ty
    }

    pub fn has_ro_aux_tree(&self) -> bool {
        self.aux_tree_type() == AuxTreeType::BSET_RO_AUX_TREE
    }

    pub fn has_rw_aux_tree(&self) -> bool {
        self.aux_tree_type() == AuxTreeType::BSET_RW_AUX_TREE
    }

    pub fn set_no_aux_tree(&mut self) {
        self.size            = 0;
        self.extra           = BSET_NO_AUX_TREE_VAL;
        self.aux_data_offset = u16::MAX;
    }

    /// Its first key's offset in the node.
    pub fn first_offset(&self) -> u16 {
        let first = self.data_offset as usize + offset_of!(Head<c::bset>, _data) / size_of::<u64>();

        invariant!(first <= u16::MAX as usize);
        u16::try_from(first).unwrap_or_else(|_| crate::invariant_failed("a bset past the last offset"))
    }

    /// Its keys' u64s.
    pub fn u64s(&self) -> usize {
        (self.end_offset - self.first_offset()) as usize
    }
}

#[bch_enum(closed)]
#[c_name(bset_aux_tree_type)]
#[repr(u32)]
pub enum AuxTreeType {
    BSET_NO_AUX_TREE,
    BSET_RO_AUX_TREE,
    BSET_RW_AUX_TREE,
}

pub const BSET_TREE_NR_TYPES: u32 = 3;

/* bset_stats.sets[]: the counts for one bset tree type */
#[repr(C)]
#[c_name(bset_stats_set)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct BsetStatsSet {
    pub nr: usize,
    pub bytes: usize,
}

#[repr(C)]
#[c_name(bset_stats)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct BsetStats {
    pub sets: [BsetStatsSet; c::BSET_TREE_NR_TYPES as usize],

    pub floats: usize,
    pub failed: usize,
}

fn pr_err(args: fmt::Arguments<'_>) {
    log_no_fs(LogLevel::ERR, &Msg(|out: &mut Printbuf| { let _ = out.write_fmt(args); }));
}

impl Keys<'_> {
    #[cold]
    pub fn bset_to_text(&self, out: &mut Printbuf, fs: Option<&Fs>, set: usize) {
        let b = self.contents();
        let t = &self.sets[set];
        let (start, last) = (t.first_offset(), t.end_offset);
        let mut k = start;

        while k < last {
            if self.buf[k as usize].to_ne_bytes()[0] == 0 {
                pr_err(format_args!("block {set} key {:5} - u64s 0? aieee!", k - start));
                break;
            }

            let _k = self.key_at(k);
            let n = k + _k.u64s() as u16;
            let mut uk = c::bkey::new();
            let sc = _k.disassemble(b, &mut uk);

            prt!(out, "block {set} key {:5}: ", k - start);
            match fs {
                Some(fs) => prt!(out, "{}", sc.to_text(fs)),
                None     => prt!(out, "{}", sc.k),
            }
            prt!(out, "\n");

            if n != last {
                let nu = self.key_at(n).unpack(b);

                if nu.p < sc.k.p {
                    pr_err(format_args!("Key skipped backwards"));
                } else if !sc.k.key_type().is_deleted() && nu.p == sc.k.p {
                    pr_err(format_args!("Duplicate keys"));
                }
            }
            k = n;
        }
    }

    #[cold]
    pub fn btree_node_bsets_to_text(&self, out: &mut Printbuf, fs: Option<&Fs>) {
        for set in 0..self.sets.len() {
            self.bset_to_text(out, fs, set);
        }
    }

    pub fn btree_node_count_keys(&self) -> BtreeNrKeys {
        let mut nr = BtreeNrKeys::default();

        for (set, t) in self.sets.iter().enumerate() {
            for (_, k) in self.set_keys(t) {
                if !k.key_type().is_deleted() {
                    nr.account_key_add(set, k);
                }
            }
        }
        nr
    }

    fn __verify_btree_nr_keys(&self, nr: &BtreeNrKeys) {
        let counted = self.btree_node_count_keys();

        assert!(counted == *nr, "btree node's key counts {nr:?}, its keys' {counted:?}");
    }

    pub(crate) fn verify_btree_nr_keys(&self) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_btree_accounting() {
            self.__verify_btree_nr_keys(&self.contents().nr);
        }
    }

    #[cold]
    #[inline(never)]
    fn insert_pos_bad(&self, msg: &str, l_name: &str, l: &BkeyP, r_name: &str, r: &BkeyP) -> ! {
        let b = self.contents();
        let mut buf = Printbuf::new();

        self.btree_node_bsets_to_text(&mut buf, None);
        prt!(&mut buf, "{msg}:\n");
        prt!(&mut buf, "{l_name} key {}\n", l.unpack(b));
        prt!(&mut buf, "{r_name} key {}\n", r.unpack(b));

        print_lines(&LogLevel::ERR.prefix(), buf.as_bytes());
        panic!("__verify_insert_pos");
    }

    fn __verify_insert_pos(&self, where_: u16, insert: &BkeyP, clobber_u64s: u16) {
        let b = self.contents();
        let t = &self.sets[self.bkey_to_bset(where_)];
        let next = where_ + clobber_u64s;

        if let Some(prev) = self.bkey_prev_all(t, where_) {
            let prev = self.key_at(prev);
            if b.bkey_iter_cmp(prev, insert).is_gt() {
                self.insert_pos_bad("prev > insert", "prev   ", prev, "insert ", insert);
            }
        }

        if next != t.end_offset {
            let next = self.key_at(next);
            if b.bkey_iter_cmp(insert, next).is_gt() {
                self.insert_pos_bad("insert > next", "insert ", insert, "next   ", next);
            }
        }
    }

    fn verify_insert_pos(&self, where_: u16, insert: &BkeyP, clobber_u64s: u16) {
        if check_lookups() {
            self.__verify_insert_pos(where_, insert, clobber_u64s);
        }
    }
}

impl Keys<'_> {
    pub fn btree_keys_stats(&self, stats: &mut BsetStats) {
        for t in self.sets {
            let ty = t.aux_tree_type() as usize;

            stats.sets[ty].nr += 1;
            stats.sets[ty].bytes += t.u64s() * size_of::<u64>();

            if let AuxTree::Ro(ro) = self.aux_tree(t) {
                stats.floats += t.size as usize - 1;
                stats.failed += ro.f[1..t.size as usize].iter().filter(|f| f.exponent == BFLOAT_FAILED).count();
            }
        }
    }
}

impl BtreeNode {
    pub fn __verify_btree_nr_keys(&self, contents: &NodeContents) {
        self.keys(contents).__verify_btree_nr_keys(&contents.nr);
    }

    #[cold]
    pub fn bfloat_to_text(&self, contents: &NodeContents, out: &mut Printbuf, k: u16) {
        let keys = self.keys(contents);
        let t = &keys.sets[keys.bkey_to_bset(k)];

        let AuxTree::Ro(ro) = keys.aux_tree(t) else { return };

        let inorder = t.bkey_to_cacheline(k);
        if inorder == 0 || inorder >= t.size as usize {
            return;
        }

        let j = ro.eytz.from_inorder(inorder);
        if k != ro.tree_to_bkey(t, j) {
            return;
        }

        if ro.f[j].exponent == BFLOAT_FAILED {
            let uk = keys.key_at(k).unpack(contents);
            prt!(out, "    failed unpacked at depth {}\n\t", j.ilog2());
            prt!(out, "{}\n", uk.p);
        }
    }
}

/* Bkey utility code */

impl Keys<'_> {
    pub fn bkey_to_bset(&self, k: u16) -> usize {
        let i = self.sets.iter().position(|t| k <= t.end_offset)
            .unwrap_or_else(|| panic!("key at {k}, past the end of the node's last set"));

        ebug_assert!(k >= self.sets[i].first_offset());
        i
    }

    /*
     * find _some_ key in the same bset as @k that precedes @k - not necessarily the
     * immediate predecessor:
     */
    fn __bkey_prev(&self, t: &BsetTree, k: u16) -> Option<u16> {
        ebug_assert!(k >= t.first_offset() && k <= t.end_offset);

        if k == t.first_offset() {
            return None;
        }

        Some(match self.aux_tree(t) {
            AuxTree::None => t.first_offset(),
            AuxTree::Ro(ro) => {
                let mut j = (t.size as usize - 1).min(t.bkey_to_cacheline(k));

                loop {
                    let p = if j != 0 {
                        let n = ro.eytz.from_inorder(j);
                        j -= 1;
                        ro.tree_to_bkey(t, n)
                    } else {
                        t.first_offset()
                    };

                    if p < k {
                        break p;
                    }
                }
            }
            AuxTree::Rw(rw) => {
                let j = rw_aux_tree_bsearch(t, rw, k);

                if j != 0 { rw[j - 1].offset } else { t.first_offset() }
            }
        })
    }

    pub fn bkey_prev_filter(&self, t: &BsetTree, orig_k: u16, min_key_type: u32) -> Option<u16> {
        let mut k = orig_k;
        let mut ret = None;

        while ret.is_none() {
            let Some(p) = self.__bkey_prev(t, k) else { break };

            ret = self.keys_between(p, k).filter(|(_, key)| key.key_type().0 >= min_key_type)
                .last().map(|(i, _)| i);
            k = p;
        }

        if check_lookups() {
            assert!(ret.is_none_or(|r| r < orig_k), "prev of key at {orig_k} at {ret:?}");

            for (i, key) in self.keys_between(ret.map_or(t.first_offset(), |r| self.bkey_p_next(r)), orig_k) {
                assert!(key.key_type().0 < min_key_type,
                        "key at {i} of type {} skipped, before key at {orig_k}", key.key_type().0);
            }
        }

        ret
    }

    pub fn bkey_prev_all(&self, t: &BsetTree, k: u16) -> Option<u16> {
        self.bkey_prev_filter(t, k, 0)
    }

}

/* Insert */

impl KeysMut<'_> {
    pub fn btree_keys_init(&mut self) {
        self.sets.clear();
        *self.nr = BtreeNrKeys::default();
    }

    pub(crate) fn bset_head_mut(&mut self, offset: u16) -> &mut Head<c::bset> {
        let len = self.buf.len();
        cast_plain_mut(plain_bytes_mut(&mut self.buf[offset as usize..]))
            .unwrap_or_else(|| panic!("no room for a bset at offset {offset} of a {len} u64 node"))
    }

    /// Every set's keys before @min and after @max gone.
    pub fn drop_keys_outside(&mut self, min: Bpos, max: Bpos) {
        for set in 0..self.sets.len() {
            let t = self.sets[set];
            let keys = self.as_keys();
            let k = keys.keys_between(t.first_offset(), t.end_offset)
                .find(|&(at, _)| keys.unpack_pos(at) >= min)
                .map_or(t.end_offset, |(at, _)| at);

            if k != t.first_offset() {
                let shift = k - t.first_offset();

                self.buf.copy_within(k as usize..t.end_offset as usize, t.first_offset() as usize);
                self.bset_head_mut(t.data_offset).set_u64s(t.u64s() - shift as usize);
                self.set_bset_end(set);
            }

            let t = self.sets[set];
            let keys = self.as_keys();
            let past = keys.keys_between(t.first_offset(), t.end_offset)
                .find(|&(at, _)| keys.unpack_pos(at) > max)
                .map(|(at, _)| at);

            if let Some(k) = past {
                self.bset_head_mut(t.data_offset).set_u64s((k - t.first_offset()) as usize);
                self.set_bset_end(set);
            }
        }
    }

    /// The bset at @offset, with its first set's seq, as a new set.
    fn bset_init(&mut self, offset: u16, seq: Le64) {
        let i = self.bset_head_mut(offset);

        plain_bytes_mut(i).fill(0);
        i.seq = seq;
        i.set_big_endian(cfg!(target_endian = "big") as _);

        let set = self.sets.push();
        self.set_bset(set, offset);
    }

    /// The node's first bset, in its header.
    pub fn bset_init_first(&mut self) {
        assert!(self.sets.is_empty(), "first bset of a node with {} already", self.sets.len());

        let offset = offset_of!(Head<c::btree_node>, keys) / size_of::<u64>();
        self.bset_init(offset as u16, Le64::new(random_u64()));
    }

    /// @bne the offset of a btree_node_entry, its bset after its csum.
    pub fn bset_init_next(&mut self, bne: u16) {
        let last_end = self.sets.last().expect("a btree node with no bsets").end_offset;

        assert!((bne as usize) < self.buf.len(), "btree node entry at {bne}, past a {} u64 node", self.buf.len());
        assert!(bne >= last_end, "btree node entry at {bne}, before the last bset's end at {last_end}");
        assert!(self.sets.len() < MAX_BSETS as usize, "a btree node's {} bsets, full", self.sets.len());

        let seq = self.as_keys().bset(&self.sets[0]).seq;
        let keys = offset_of!(Head<c::btree_node_entry>, keys) / size_of::<u64>();
        self.bset_init(bne + keys as u16, seq);
    }

    pub fn bset_insert(&mut self, where_: u16, insert: &BkeyI, clobber_u64s: u16) {
        let f = self.format;
        let set = self.sets.len() - 1;
        let insert_p: &BkeyP = insert.into();
        let mut packed = c::bkey_packed::default();

        self.as_keys().bset_verify_rw_aux_tree(&self.sets[set]);
        self.as_keys().verify_insert_pos(where_, insert_p, clobber_u64s);

        let words = cast_plain_slice_mut(plain_bytes_mut(&mut packed)).expect("a bkey_packed is whole u64s");
        let (src_u64s, src_key): (u16, &[u64]) = if f.pack_key(words, &insert.k) {
            let key: &[u64] = cast_plain_slice(plain_bytes(&packed)).expect("a bkey_packed is whole u64s");
            (packed.u64s as u16, &key[..f.key_u64s as usize])
        } else {
            (insert.k.u64s as u16, &insert_p.as_slice()[..BKEY_U64S])
        };

        let (w, clobber, u64s) = (where_ as usize, clobber_u64s as usize, src_u64s as usize);

        if src_u64s != clobber_u64s {
            let t = self.sets[set];

            ebug_assert!(t.u64s() as isize >= clobber as isize - u64s as isize);

            self.buf.copy_within(w + clobber..t.end_offset as usize, w + u64s);
            let bset_u64s = t.u64s() + u64s - clobber;
            self.bset_head_mut(t.data_offset).set_u64s(bset_u64s);
            self.set_bset_end(set);
        }

        self.buf[w..w + src_key.len()].copy_from_slice(src_key);
        self.buf[w + src_key.len()..w + u64s].copy_from_slice(insert_p.val(f));

        if !insert.k.key_type().is_deleted() {
            self.nr.account_key_add(set, key_at(self.buf, where_));
        }

        if src_u64s != clobber_u64s {
            self.bset_fix_lookup_table(set, where_, clobber_u64s, src_u64s);
        }

        self.verify_btree_nr_keys();
    }

    pub fn bset_delete(&mut self, where_: u16, clobber_u64s: u16) {
        let set = self.sets.len() - 1;
        let t = self.sets[set];
        let (w, clobber) = (where_ as usize, clobber_u64s as usize);

        self.as_keys().bset_verify_rw_aux_tree(&t);

        ebug_assert!(t.u64s() >= clobber);

        self.buf.copy_within(w + clobber..t.end_offset as usize, w);
        let bset_u64s = t.u64s() - clobber;
        self.bset_head_mut(t.data_offset).set_u64s(bset_u64s);
        self.set_bset_end(set);

        self.bset_fix_lookup_table(set, where_, clobber_u64s, 0);
    }

    pub(crate) fn verify_btree_nr_keys(&self) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_btree_accounting() {
            self.as_keys().__verify_btree_nr_keys(self.nr);
        }
    }
}

/* Lookup */

impl Keys<'_> {
    /* packed or unpacked */
    #[inline]
    pub(super) fn cmp_p_or_unp(&self, l: u16, r_packed: Option<&BkeyP>, r: &Bpos) -> Ordering {
        ebug_assert!(r_packed.is_none_or(|r| r.is_packed()));

        let k = self.key_at(l);
        match r_packed {
            Some(r_packed) if likely(k.is_packed()) => self.contents().cmp_packed_format_checked(k, r_packed),
            _ => self.unpack_pos(l).cmp(r),
        }
    }

    fn bkey_iter_cmp_p_or_unp(&self, l: u16, r_packed: Option<&BkeyP>, r: &Bpos) -> Ordering {
        self.cmp_p_or_unp(l, r_packed, r)
            .then_with(|| if self.key_at(l).key_type().is_deleted() { Ordering::Less } else { Ordering::Equal })
    }

    /* These assume r (the search key) is not a deleted key: */
    pub(crate) fn bkey_iter_pos_cmp(&self, l: u16, r: &Bpos) -> Ordering {
        self.unpack_pos(l).cmp(r)
            .then_with(|| if self.key_at(l).key_type().is_deleted() { Ordering::Less } else { Ordering::Equal })
    }
}

impl NodeContents {
    /*
     * When keys compare equal, deleted keys compare first:
     *
     * XXX: only need to compare pointers for keys that are both within a
     * btree_node_iterator - we need to break ties for prev() to work correctly
     */
    pub(super) fn bkey_iter_cmp(&self, l: &BkeyP, r: &BkeyP) -> Ordering {
        self.cmp_packed(l, r)
            .then_with(|| r.key_type().is_deleted().cmp(&l.key_type().is_deleted()))
            .then_with(|| l.as_ffi().cmp(&r.as_ffi()))
    }
}

impl Keys<'_> {
    pub(super) fn __bset_search(&self, t: &BsetTree, search: &Bpos,
                                     lossy_packed_search: &c::bkey_packed) -> u16 {
        /*
         * First, we search for a cacheline, then lastly we do a linear search
         * within that cacheline.
         *
         * To search for the cacheline, there's three different possibilities:
         *  * The set is too small to have a search tree, so we just do a linear
         *    search over the whole set.
         *  * The set is the one we're currently inserting into; keeping a full
         *    auxiliary search tree up to date would be too expensive, so we
         *    use a much simpler lookup table to do a binary search -
         *    bset_search_write_set().
         *  * Or we use the auxiliary search tree we constructed earlier -
         *    bset_search_tree()
         */

        match self.aux_tree(t) {
            AuxTree::None   => t.first_offset(),
            AuxTree::Rw(rw) => bset_search_write_set(rw, search),
            AuxTree::Ro(ro) => bset_search_tree(self, t, ro, search, lossy_packed_search),
        }
    }

    pub(super) fn bset_search_linear(&self, t: &BsetTree, search: &Bpos,
                                          packed_search: Option<&BkeyP>,
                                          lossy_packed_search: &BkeyP, mut m: u16) -> u16 {
        let end = t.end_offset;

        m = self.keys_between(m, end)
            .find(|&(at, _)| !self.bkey_iter_cmp_p_or_unp(at, Some(lossy_packed_search), search).is_lt())
            .map_or(end, |(at, _)| at);

        if packed_search.is_none() {
            m = self.keys_between(m, end)
                .find(|&(at, _)| !self.bkey_iter_pos_cmp(at, search).is_lt())
                .map_or(end, |(at, _)| at);
        }

        if check_lookups() {
            if let Some(prev) = self.bkey_prev_all(t, m) {
                assert!(self.bkey_iter_cmp_p_or_unp(prev, packed_search, search).is_lt(),
                        "bset search for {search}: key at {prev}, before the result at {m}, isn't before it");
            }
        }

        m
    }
}

impl NodeContents {
    /// The format, and what's derived from it: the key bits and the unpack
    /// constants; the aux trees, built for the old one, go.
    pub fn set_format(&mut self, f: c::bkey_format) {
        self.format      = f;
        self.nr_key_bits = f.key_bits() as u8;

        self.compute_unpack_consts();

        /* the compiled unpack is off: bkey.h's HAVE_BCACHEFS_COMPILED_UNPACK */
        self.unpack_fn_len = 0;

        for t in &mut self.set {
            t.set_no_aux_tree();
        }
    }
}
