// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

/// The checks behind bch2_debug_check_bset_lookups, in debug builds.
fn check_lookups() -> bool {
    cfg!(CONFIG_BCACHEFS_DEBUG) && crate::debug::params::debug_check_bset_lookups()
}

pub mod aux_tree;
pub mod bset;
pub mod insert;
pub mod iter;
pub mod keys;
pub mod sort;

use crate::btree::bkey::{BkeyI, BkeyP, BkeySC, BkeyV, BKEY_U64S, POS_MIN, SPOS_MAX};
use crate::btree::cache::NodeBufs;
use crate::btree::path::LockedNode;
use crate::c;
use crate::cstructs::c as cs;
use crate::data_structures::dst_room::DstRoom;
use crate::fs::Fs;
use crate::journal::pins::Pin;
use crate::locking::LockedBy;
use crate::util::projections::{Publish, Volatile};
use crate::util::vstructs::Head;
use crate::util::{Printbuf, ToText};
use crate::data_structures::list::ListLinks;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::offset_of;
use core::ops::{ControlFlow, Range};
use core::ptr;
use bset::{BsetTree, BSET_CACHELINE};
use iter::KeyIter;
use crate::format::BkeyType;
use crate::util::alloc::Flags;
use cstruct_macros::{Init, MaybeZeroable};
use pin_init::pin_data;

/* Btree nodes: */

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, MaybeZeroable)]
pub struct BtreeNrKeys {
    /*
     * Amount of live metadata (i.e. size of node after a compaction) in
     * units of u64s
     */
    pub live_u64s: u16,
    pub bset_u64s: [u16; cs::MAX_BSETS as usize],

    /* live keys only: */
    pub packed_keys: u16,
    pub unpacked_keys: u16,
}

impl BtreeNrKeys {
    fn account_key(&mut self, set: usize, k: &BkeyP, sign: i16) {
        let u64s = k.u64s() as i16 * sign;

        self.live_u64s = self.live_u64s.wrapping_add_signed(u64s);
        self.bset_u64s[set] = self.bset_u64s[set].wrapping_add_signed(u64s);
        if k.is_packed() {
            self.packed_keys = self.packed_keys.wrapping_add_signed(sign);
        } else {
            self.unpacked_keys = self.unpacked_keys.wrapping_add_signed(sign);
        }
    }

    pub fn account_key_add(&mut self, set: usize, k: &BkeyP) {
        self.account_key(set, k, 1)
    }

    pub fn account_key_drop(&mut self, set: usize, k: &BkeyP) {
        self.account_key(set, k, -1)
    }

    /// The live u64s, the keys repacked from @old_f to @new_f.
    pub fn u64s_with_format(&self, old_f: &c::bkey_format, new_f: &c::bkey_format) -> usize {
        let new_key_u64s = i32::from(new_f.key_u64s);
        let delta = (new_key_u64s - i32::from(old_f.key_u64s)) * i32::from(self.packed_keys) +
                    (new_key_u64s - BKEY_U64S as i32) * i32::from(self.unpacked_keys);
        let u64s = i32::from(self.live_u64s) + delta;

        assert!(u64s >= 0, "{} live u64s, repacked {delta} more", self.live_u64s);
        u64s as usize
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct BkeyUnpackField {
    pub byte_offset: i8,
    pub shift_right: u8, /* 64 - bits, or 64 if field has no bits in packed */
}

#[pin_data]
#[repr(C)]
#[derive(Init)]
#[init_args(fs: &Fs, lock_flags: c::SixLockInitFlags, gfp: Flags)]
pub struct BtreeNode {
    #[pin]
    #[init(cs::btree_bkey_cached_common::init(false, lock_flags, gfp))]
    pub c: cs::btree_bkey_cached_common,

    pub hash_val: core::cell::UnsafeCell<u64>,

    pub flags: cs::AtomicBtreeNodeFlags,
    /*
     * XXX: written and whiteout_u64s are the node writer's: the thread that
     * cleared BTREE_NODE_dirty, which may hold only a read lock. They want a
     * Volatile variant whose Writer has a private constructor - clearing the
     * dirty bit - and borrows the guard on c.lock. With btree/write.c's
     * conversion.
     */
    pub written: Volatile<u16>,
    pub whiteout_u64s: Volatile<u16>,

    #[init(fs.opts.btree_node_size.get().ilog2() as u8)]
    pub byte_order: u8,

    /* Key/pointer for this btree node: its published half */
    #[init(LockedBy::new(NodeContents::new(), PublishedKey::new()))]
    pub contents: LockedBy<NodeContents, PublishedKey>,

    /* the journal pins of its current write and the one before */
    pub writes: [Pin; 2],

    /*
     * XXX: add a delete sequence number, so when bch2_btree_node_relock()
     * fails because the lock sequence number has changed - i.e. the
     * contents were modified - we can still relock the node if it's still
     * the one we want, without redoing the traversal
     */

    /*
     * For asynchronous splits/interior node updates:
     * When we do a split, we allocate new child nodes and update the parent
     * node to point to them: we update the parent in memory immediately,
     * but then we must wait until the children have been written out before
     * the update to the parent can be written - this is a list of the
     * btree_updates that are blocking this node from being
     * written:
     */
    pub(crate) write_blocked: LockedBy<crate::btree::interior::WriteBlocked, (), crate::btree::interior::UpdatesLockAt>,

    /*
     * Also for asynchronous splits/interior node updates:
     * If a btree node isn't reachable yet, we don't want to kick off
     * another write - because that write also won't yet be reachable and
     * marking it as completed before it's reachable would be incorrect:
     */
    #[init(crate::btree::interior::WillMakeReachable::default())]
    pub will_make_reachable: crate::btree::interior::WillMakeReachable,

    /* lru list */
    pub list: ListLinks,

    pub cache_state: LockedBy<cs::BtreeNodeCacheState, (), crate::btree::types::BtreeCacheLockAt>,

    /* its slot in bc->nodes */
    pub nodes_idx: u32,

    // never moved, nor freed while the fs runs: what linking it promises -
    // in a PhantomData, as a bare PhantomPinned isn't FFI-safe
    _pin: PhantomData<PhantomPinned>,
}

/// A node's keys, their buffer and their format: the node lock's -
/// written with it held for write, read with it held any way.
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct NodeContents {
    /* data swapped for a sort's bounce buffer, with the node held for write */
    pub bufs: NodeBufs,

    pub nsets: u8,
    pub nr_key_bits: u8,

    pub format: cs::bkey_format,

    /*
     * Per-field unpack constants, derived from @format at node init.
     * Extract each field with:
     *
     *   field = (load_8_unaligned(bytes + byte_offset) >> (64 - bits))
     *           + field_offset
     *
     * Load position chosen so the field ends at the top of the loaded
     * value (load_offset + 8 == byte after field's MSB byte); junk from
     * earlier-in-memory fields lands in the low bits and shifts off.
     *
     * byte_offset is signed: for a field near the start of @in, the
     * load can need to start before @in. The byte(s) before @in are
     * always valid memory in the callers we care about (bset payload
     * after the bset header, or other bkeys in the same bset).
     *
     * Only handles formats where every field's MSB sits at a byte
     * boundary (field_msb_bit % 8 == 7). bch2_bkey_format_done()
     * rounds fields up to byte width when there are spare bits, so
     * this is the common case. Formats too tight to byte-align take
     * the slow path via byte_aligned_fields = false.
     */
    pub byte_aligned_fields: bool,
    pub unpack: [BkeyUnpackField; cs::BkeyField::BKEY_NR_FIELDS as usize],

    /*
     * Sets of sorted keys - the real btree node - plus a binary search tree
     *
     * set[0] is special; set[0]->tree, set[0]->prev and set[0]->data point
     * to the memory we have allocated for this btree node. Additionally,
     * set[0]->data points to the entire btree node as it exists on disk.
     */
    pub set: [BsetTree; cs::MAX_BSETS as usize],

    pub nr: BtreeNrKeys,
    /*
     * Hints, written holding the node only intent locked - by merge - and read
     * with it locked any way.
     */
    pub sib_u64s: [Volatile<u16>; 2],
    pub unpack_fn_len: u8,
}

pub type NodeKey = DstRoom<BkeyI, [u64; NODE_KEY_U64S]>;

const NODE_KEY_U64S: usize = BKEY_U64S + cs::BKEY_BTREE_PTR_VAL_U64s_MAX as usize;

/// A node's key, published: stored word by word with the node held for
/// write, read with it held any way - and peeked at without it.
#[repr(transparent)]
pub struct PublishedKey([Volatile<u64>; NODE_KEY_U64S]);

/// A PublishedKey, as its node's write lock holder has it.
#[repr(transparent)]
pub struct PublishedKeyWriter(PublishedKey);

// its words' stores are atomic
impl Publish for PublishedKey {
    type Writer = PublishedKeyWriter;
}

impl PublishedKey {
    /// A new node's: an empty btree_ptr.
    fn new() -> Self {
        let mut k = NodeKey::default();
        k.k.set_key_type(BkeyType::btree_ptr);
        PublishedKey(core::array::from_fn(|i| Volatile::new(k.elems()[i])))
    }

    /// A copy, unlocked: torn, if an update's storing it meanwhile.
    pub fn peek(&self) -> NodeKey {
        let mut k = NodeKey::default();
        for (v, w) in k.elems_mut().iter_mut().zip(&self.0) {
            *v = w.get();
        }
        k
    }
}

impl PublishedKeyWriter {
    /// @k's room, every word of it.
    pub fn store(&self, k: &NodeKey) {
        for (w, &v) in self.0.0.iter().zip(k.elems()) {
            w.set(v);
        }
    }
}

impl<M> LockedNode<'_, BtreeNode, M> {
    pub fn to_text<'f>(&self, fs: &'f Fs) -> BtreeNodeToText<'_, 'f> {
        let b = self.held();
        b.node().to_text(b.contents(), fs)
    }

    pub fn ondisk_to_text<'f>(&self, fs: &'f Fs) -> BtreeNodeOndiskToText<'_, 'f> {
        let b = self.held();
        BtreeNodeOndiskToText { b: b.node(), contents: b.contents(), fs }
    }
}

impl<'b, 'f> BtreeNode {
    pub fn to_text(&'b self, contents: &'b NodeContents, fs: &'f Fs) -> BtreeNodeToText<'b, 'f> {
        BtreeNodeToText { b: self, contents, fs }
    }

    pub fn pos_to_text(&'b self, fs: &'f Fs) -> BtreeNodePosToText<'b, 'f> {
        BtreeNodePosToText { b: self, fs }
    }
}

impl BtreeNode {
    /// The node's contents, on the caller's word that it holds the node lock,
    /// until btree paths hand out its guards.
    ///
    /// # Safety
    /// The node lock is held, any way, for as long as the reference lives, and
    /// nothing has them mutably.
    pub unsafe fn contents(&self) -> &NodeContents {
        unsafe { self.contents.protected() }
    }

    /// # Safety
    /// The node lock is held for write, for as long as the reference lives,
    /// and nothing else refers to them.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn contents_mut(&self) -> &mut NodeContents {
        unsafe { self.contents.protected_mut() }
    }

    fn key_room<'a>(&'a self, contents: &'a NodeContents) -> &'a NodeKey {
        self.own_contents(contents);
        unsafe { &*ptr::from_ref(&*self.contents).cast::<NodeKey>() }
    }

    pub fn key<'a>(&'a self, contents: &'a NodeContents) -> &'a BkeyI {
        self.key_room(contents)
    }

    /// Its key changed by @f, by @contents' lock held for write: on a copy,
    /// stored whole, as a peek may be reading it meanwhile.
    pub fn update_key<R>(&self, contents: &mut NodeContents, f: impl FnOnce(&mut NodeKey) -> R) -> R {
        let mut k = *self.key_room(contents);
        let ret = f(&mut k);
        unsafe { self.contents.published_writer() }.store(&k);
        ret
    }

    pub fn set_key(&self, contents: &mut NodeContents, k: impl BkeyV) {
        self.update_key(contents, |room| room.set_key(k))
    }

    /// Its key, unlocked: a copy an update may tear.
    pub fn key_peek(&self) -> NodeKey {
        self.contents.peek()
    }

    pub fn reset_sib_u64s(&self, contents: &NodeContents) {
        let live = contents.nr.live_u64s;
        contents.sib_u64s[0].set(if contents.data().min_key != POS_MIN { live } else { u16::MAX });
        contents.sib_u64s[1].set(if self.key(contents).k.p != SPOS_MAX { live } else { u16::MAX });
    }

    pub fn is_fake(&self) -> bool {
        self.flags.fake()
    }

    /// Not yet reachable: an interior update in flight will make it so.
    pub fn will_make_reachable(&self) -> bool {
        self.flags.will_make_reachable()
    }

    /// The node's keys, unpacked, in order.
    pub fn for_each_key<F>(&self, contents: &NodeContents, mut f: F) -> ControlFlow<()>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        let keys = self.keys(contents);
        let mut node_iter = KeyIter::default();
        let mut unpacked = c::bkey::new();

        node_iter.init_from_start(&keys);

        while let Some(k) = node_iter.peek_unpack(&keys, &mut unpacked) {
            f(k)?;
            node_iter.advance(&keys);
        }
        ControlFlow::Continue(())
    }

    /// Every bset's live keys added to @s.
    pub fn format_add_keys(&self, contents: &NodeContents, s: &mut c::bkey_format_state) {
        let keys = self.keys(contents);
        for t in contents.sets() {
            for (i, k) in keys.set_keys(t) {
                if !k.key_type().is_deleted() {
                    s.add_key(&keys.unpack(i));
                }
            }
        }
    }

    /* Calculate ideal packed bkey format for new btree nodes: */
    pub fn calc_format(&self, contents: &NodeContents) -> c::bkey_format {
        let data = contents.data();
        let mut s = c::bkey_format_state::new();
        s.add_pos(data.min_key);
        s.add_pos(data.max_key);
        self.format_add_keys(contents, &mut s);
        s.done()
    }
}

impl NodeContents {
    /// A new node's: no keys, no buffer.
    fn new() -> Self {
        ::pin_init::zeroed()
    }

    pub fn data(&self) -> &Head<cs::btree_node> {
        unsafe { &*self.bufs.data.cast() }
    }

    pub fn data_mut(&mut self) -> &mut Head<cs::btree_node> {
        unsafe { &mut *self.bufs.data.cast() }
    }

    pub fn sets(&self) -> &[BsetTree] {
        invariant!(self.nsets as usize <= self.set.len());
        &self.set[..self.nsets as usize]
    }

    pub fn sets_mut(&mut self) -> &mut [BsetTree] {
        invariant!(self.nsets as usize <= self.set.len());
        &mut self.set[..self.nsets as usize]
    }

    /// A new set, at the end: its index.
    pub fn push_set(&mut self) -> usize {
        let i = self.nsets as usize;
        assert!(i < self.set.len(), "a btree node with {i} bsets, of at most {}", self.set.len());
        self.nsets += 1;
        i
    }

    /// No sets: none with a bset, none with an aux tree.
    pub fn clear_sets(&mut self) {
        self.nsets = 0;
        for t in self.set.iter_mut() {
            t.data_offset = u16::MAX;
        }
        self.set_no_aux_tree(0);
    }

    /// From set @from on, to the last there could be: none has an aux tree.
    pub fn set_no_aux_tree(&mut self, from: usize) {
        for t in &mut self.set[from..] {
            t.set_no_aux_tree();
        }
    }

    pub fn last_set(&self) -> &BsetTree {
        self.sets().last().expect("a btree node with no bsets")
    }

    /*
     * If we have MAX_BSETS (3) bsets, should we sort them all down to just one?
     *
     * The first bset is going to be of similar order to the size of the node, the
     * last bset is bounded by btree_write_set_buffer(), which is set to keep the
     * memmove on insert from being too expensive: the middle bset should, ideally,
     * be the geometric mean of the first and the last.
     *
     * Returns true if the middle bset is greater than that geometric mean:
     */
    pub fn should_compact_all(&self, fs: &Fs) -> bool {
        let mid_u64s_bits = (fs.btree_max_u64s().ilog2() + WRITE_SET_U64S_BITS) / 2;
        self.set[1].u64s() > 1 << mid_u64s_bits
    }
}

impl BtreeNode {
    pub fn buf_bytes(&self) -> usize {
        invariant!((self.byte_order as u32) < usize::BITS);
        1 << self.byte_order
    }

    /// Whether a node of @u64s of keys fits its buffer: strictly less than,
    /// as bch2_btree_node_format_fits() and the write's slop have it.
    pub fn fits_u64s(&self, u64s: usize) -> bool {
        size_of::<Head<c::btree_node>>() + u64s * size_of::<u64>() < self.buf_bytes()
    }

    pub fn keys_cachelines(&self) -> usize {
        self.buf_bytes() / BSET_CACHELINE
    }

    pub fn aux_data_bytes(&self) -> usize {
        self.keys_cachelines() * size_of::<u64>()
    }

    pub fn aux_data_u64s(&self) -> usize {
        self.aux_data_bytes() / size_of::<u64>()
    }
}

pub const WRITE_SET_U64S_BITS: u32 = 9;

/*
 * Could buffer up larger amounts of keys for btrees with larger keys,
 * pending benchmarking:
 */
const WRITE_SET_BUFFER: usize = 8 << WRITE_SET_U64S_BITS;

impl BtreeNode {
    pub fn written_end(&self) -> usize {
        self.written.get() as usize * (512 / size_of::<u64>())
    }

    pub fn bset_written(&self, t: &BsetTree) -> bool {
        (t.data_offset as usize) < self.written_end()
    }

    pub fn unwritten_whiteouts(&self) -> Range<usize> {
        let end = self.buf_bytes() / size_of::<u64>();
        end - self.whiteout_u64s.get() as usize..end
    }

    pub fn u64s_remaining(&self, end: usize) -> isize {
        let total = self.buf_bytes() / size_of::<u64>();
        /* Always leave one extra u64, slack for varint::decode_fast(): */
        total as isize - (end + self.whiteout_u64s.get() as usize + 1) as isize
    }

    /// Room after the last bset, none once it's written:
    /// bch2_btree_keys_u64s_remaining().
    pub fn keys_u64s_remaining(&self, contents: &NodeContents) -> usize {
        let t = contents.last_set();
        let remaining = self.u64s_remaining(t.end_offset as usize);
        assert!(remaining >= 0, "a node's keys past its buffer, by {} u64s", -remaining);

        if self.bset_written(t) { 0 } else { remaining as usize }
    }

    /// Whether a key of @u64s goes in without a split, or a rewrite first:
    /// bch2_btree_node_insert_fits().
    pub fn insert_fits(&self, contents: &NodeContents, u64s: usize) -> bool {
        !self.flags.need_rewrite() && u64s <= self.keys_u64s_remaining(contents)
    }

    /*
     * Will a new_key_u64s key fit after we compact @b down to a single sorted
     * bset? Models node_write()'s space accounting exactly: each bset
     * write rounds up to block_bytes(c), so a node whose live data rounds up to
     * fill the entire sector budget is born exhausted post-compact - no room for
     * a follow-on bset to land the new key, and the insert path will immediately
     * trigger another btree_split. Caller must split in that case.
     *
     * +8 in each term matches the varint::decode_fast() slack the write path
     * adds before round_up.
     */
    pub fn compact_fits(&self, contents: &NodeContents, fs: &Fs, new_key_u64s: usize) -> bool {
        let sectors = |bytes: usize| (bytes as u64).next_multiple_of(fs.block_bytes()) >> 9;

        let initial  = size_of::<Head<c::btree_node>>() + usize::from(contents.nr.live_u64s) * size_of::<u64>() + 8;
        let followon = size_of::<Head<c::btree_node_entry>>() + new_key_u64s * size_of::<u64>() + 8;

        sectors(initial) + sectors(followon) <= u64::from(fs.btree_sectors())
    }

    /// Whether, @d u64s from now, it's small enough to merge with a sibling:
    /// btree_node_needs_merge().
    pub fn needs_merge(&self, contents: &NodeContents, fs: &Fs, d: i32) -> bool {
        let sib_u64s = contents.sib_u64s[0].get().min(contents.sib_u64s[1].get());
        !crate::debug::params::btree_node_merging_disabled() &&
            i32::from(sib_u64s) + d <= i32::from(fs.btree.foreground_merge_threshold)
    }

    pub fn want_new_bset(&self, contents: &NodeContents, fs: &Fs) -> Option<u16> {
        let t = contents.last_set();
        let bne = self.written_end().max(t.end_offset as usize);
        let keys = (offset_of!(Head<c::btree_node_entry>, keys) + offset_of!(Head<c::bset>, _data)) / size_of::<u64>();

        let want = if self.bset_written(t) {
            self.written.get() as u32 + fs.block_sectors() <= fs.btree_sectors()
        } else {
            t.u64s() * size_of::<u64>() > WRITE_SET_BUFFER &&
                self.u64s_remaining(bne + keys) > (WRITE_SET_BUFFER / size_of::<u64>()) as isize
        };
        want.then(|| u16::try_from(bne).expect("a bset offset in a node's u16 range"))
    }

    pub fn current_write(&self) -> &Pin {
        &self.writes[self.flags.write_idx() as usize]
    }

    pub fn prev_write(&self) -> &Pin {
        &self.writes[self.flags.write_idx() as usize ^ 1]
    }
}

pub struct BtreeNodeToText<'b, 'f> {
    b:        &'b BtreeNode,
    contents: &'b NodeContents,
    fs:       &'f Fs,
}

impl ToText for BtreeNodeToText<'_, '_> {
    fn to_text(&self, out: &mut Printbuf) {
        crate::btree::cache::btree_node_to_text(out, self.fs, self.b, self.contents)
    }
}
crate::display_via_to_text!(BtreeNodeToText<'_, '_>);

pub struct BtreeNodeOndiskToText<'b, 'f> {
    b:        &'b BtreeNode,
    contents: &'b NodeContents,
    fs:       &'f Fs,
}

impl ToText for BtreeNodeOndiskToText<'_, '_> {
    fn to_text(&self, out: &mut Printbuf) {
        crate::btree::read::node_ondisk_to_text(out, self.fs, self.b, self.contents)
    }
}
crate::display_via_to_text!(BtreeNodeOndiskToText<'_, '_>);

pub struct BtreeNodePosToText<'b, 'f> {
    b:  &'b BtreeNode,
    fs: &'f Fs,
}

impl ToText for BtreeNodePosToText<'_, '_> {
    fn to_text(&self, out: &mut Printbuf) {
        crate::btree::cache::bch2_btree_pos_to_text(out, self.fs, self.b)
    }
}
crate::display_via_to_text!(BtreeNodePosToText<'_, '_>);
