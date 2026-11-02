// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

pub mod bset;
pub mod iter;

use crate::btree::bkey::{BkeyI, BkeySC};
use crate::c;
use crate::cstructs::c as cs;
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::bitmap::AtomicBitflags;
use crate::util::projections::Volatile;
use crate::util::vstructs::Head;
use crate::util::{Printbuf, ToText};
use core::ffi::c_void;
use core::marker::PhantomData;
use core::ops::ControlFlow;
use core::sync::atomic::AtomicUsize;
use bset::BsetTree;
use iter::KeyIter;
use cstruct_macros::{c_name, CStruct};

/* Btree nodes: */

#[repr(C)]
#[c_name(btree_nr_keys)]
#[derive(Clone, Copy, Default, CStruct)]
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

#[repr(C)]
#[c_name(bkey_unpack_field)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct BkeyUnpackField {
    pub byte_offset: i8,
    pub shift_right: u8, /* 64 - bits, or 64 if field has no bits in packed */
}

#[repr(C)]
#[c_name(btree)]
#[derive(CStruct)]
pub struct BtreeNode {
    pub c: cs::btree_bkey_cached_common,

    pub hash: cs::rhash_head,
    pub hash_val: u64,

    #[c("unsigned long flags")]
    pub flags: AtomicBitflags<cs::btree_flags>,
    /*
     * XXX: written and whiteout_u64s are the node writer's: the thread that
     * cleared BTREE_NODE_dirty, which may hold only a read lock. They want a
     * Volatile variant whose Writer has a private constructor - clearing the
     * dirty bit - and borrows the guard on c.lock. With btree/write.c's
     * conversion.
     */
    pub written: Volatile<u16>,
    pub nsets: u8,
    pub nr_key_bits: u8,
    pub version_ondisk: u16,

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
    pub unpack: [BkeyUnpackField; cs::BKEY_NR_FIELDS as usize],

    pub data: *mut Head<cs::btree_node>,
    pub aux_data: *mut c_void,

    /*
     * Sets of sorted keys - the real btree node - plus a binary search tree
     *
     * set[0] is special; set[0]->tree, set[0]->prev and set[0]->data point
     * to the memory we have allocated for this btree node. Additionally,
     * set[0]->data points to the entire btree node as it exists on disk.
     */
    pub set: [BsetTree; cs::MAX_BSETS as usize],

    pub nr: BtreeNrKeys,
    pub sib_u64s: [u16; 2],
    pub whiteout_u64s: Volatile<u16>,
    pub byte_order: u8,
    pub unpack_fn_len: u8,

    pub writes: [cs::btree_write; 2],

    /* Key/pointer for this btree node */
    pub key: cs::bkey_i,
    pub key_pad: [u64; cs::BKEY_BTREE_PTR_VAL_U64s_MAX],

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
    pub write_blocked: cs::list_head,

    /*
     * Also for asynchronous splits/interior node updates:
     * If a btree node isn't reachable yet, we don't want to kick off
     * another write - because that write also won't yet be reachable and
     * marking it as completed before it's reachable would be incorrect:
     */
    #[c("unsigned long will_make_reachable")]
    pub will_make_reachable: AtomicUsize,

    pub ob: cs::open_buckets,

    /* lru list */
    pub list: cs::list_head,

    pub cache_state: cs::btree_node_cache_state,
}
c_default!(BtreeNode);

impl<'b, 'f> BtreeNode {
    pub fn to_text(&'b self, fs: &'f Fs) -> BtreeNodeToText<'b, 'f> {
        BtreeNodeToText { b: self, fs }
    }

    pub fn ondisk_to_text(&'b self, fs: &'f Fs) -> BtreeNodeOndiskToText<'b, 'f> {
        BtreeNodeOndiskToText { b: self, fs }
    }
}

impl BtreeNode {
    /// The node's key, all of it - its value runs past bkey_i, into key_pad.
    pub fn key_i(&self) -> &BkeyI {
        // SAFETY: the node's key, from a pointer to all of the node
        unsafe { &*BkeyI::from_raw(&raw const self.key) }
    }

    pub fn is_fake(&self) -> bool {
        self.flags.get(c::btree_flags::BTREE_NODE_fake)
    }

    /// Not yet reachable: an interior update in flight will make it so.
    pub fn will_make_reachable(&self) -> bool {
        self.flags.get(c::btree_flags::BTREE_NODE_will_make_reachable)
    }

    /// The node's keys, unpacked, in order.
    pub fn for_each_key<F>(&self, mut f: F) -> ControlFlow<()>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        let b = self as *const _ as *mut BtreeNode;
        let mut node_iter = KeyIter::default();
        let mut unpacked: c::bkey = pin_init::Zeroable::zeroed();

        unsafe { bset::bch2_btree_node_iter_init_from_start(&mut node_iter, b) };

        loop {
            let k = unsafe {
                bset::bch2_btree_node_iter_peek_unpack(&mut node_iter, b, &mut unpacked)
            };
            if k.k.is_null() {
                return ControlFlow::Continue(());
            }
            if f(BkeySC {
                k:    unsafe { &*k.k },
                v:    k.v,
                iter: PhantomData,
                val:  PhantomData,
            }).is_break() {
                return ControlFlow::Break(());
            }
            unsafe { bset::bch2_btree_node_iter_advance(&mut node_iter, b) };
        }
    }
}


pub struct BtreeNodeToText<'b, 'f> {
    b:  &'b BtreeNode,
    fs: &'f Fs,
}

impl ToText for BtreeNodeToText<'_, '_> {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_btree_node_to_text(out.as_raw(), self.fs.as_ptr(), self.b) }
    }
}
crate::display_via_to_text!(BtreeNodeToText<'_, '_>);

pub struct BtreeNodeOndiskToText<'b, 'f> {
    b:  &'b BtreeNode,
    fs: &'f Fs,
}

impl ToText for BtreeNodeOndiskToText<'_, '_> {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_btree_node_ondisk_to_text(out.as_raw(), self.fs.as_ptr(), self.b) }
    }
}
crate::display_via_to_text!(BtreeNodeOndiskToText<'_, '_>);
