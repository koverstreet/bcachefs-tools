// SPDX-License-Identifier: GPL-2.0

//! A node's keys, its buffers as slices: the node holds thin pointers, and
//! a view is made of them once per operation, not per access. Keys and bsets
//! are referred to, as in C, by their offsets in u64s from the node's start.

use super::bset::{BsetTree, BSET_NO_AUX_TREE_VAL};
use super::{BtreeNode, BtreeNrKeys};
use crate::types::L1_CACHE_BYTES;
use crate::btree::bkey::BkeyP;
use crate::c;
use crate::cstructs::c as cs;
use crate::format::Bpos;
use crate::util::vstructs::{cast_plain, from_bytes, plain_bytes, Head};
use core::ops::{Deref, DerefMut};

pub struct Keys<'a> {
    /// For C's unpack, which reads only the format fields.
    node:              *const BtreeNode,
    pub sets:          &'a [BsetTree],
    /// The node's buffer, 1 << byte_order bytes: its bsets, and the
    /// unwritten whiteouts at its end.
    pub buf:           &'a [u64],
    /// The bsets' lookup tables, each at its set's aux_data_offset.
    pub aux:           &'a [u64],
    pub unpack_fn_len: u8,
}

/// Keys, to change: the node's parts, borrowed apart.
pub struct KeysMut<'a> {
    /// For C's unpack, which reads only the format fields, borrowed below.
    pub(super) node:   *const BtreeNode,
    pub format:        &'a c::bkey_format,
    pub nr_key_bits:   u8,
    pub unpack_fn_len: u8,
    pub sets:          Sets<'a>,
    pub nr:            &'a mut BtreeNrKeys,
    pub buf:           &'a mut [u64],
    pub aux:           &'a mut [u64],
}

/// The node's sets, the first nsets of them: nsets and the array are apart
/// in BtreeNode.
pub struct Sets<'a> {
    set: &'a mut [BsetTree; cs::MAX_BSETS as usize],
    nr:  &'a mut u8,
}

impl BtreeNode {
    /// The aux trees' cachelines are the buffer's, by offset: rounding an
    /// offset is rounding the address only if the buffer is L1-aligned.
    fn buf_aligned(&self) {
        ebug_assert!(self.data as usize % L1_CACHE_BYTES == 0,
                     "btree node buffer at {:p}, not {L1_CACHE_BYTES} byte aligned", self.data);
    }

    #[inline]
    pub fn keys(&self) -> Keys<'_> {
        self.buf_aligned();
        unsafe {
            Keys {
                node:          self,
                sets:          self.sets(),
                buf:           core::slice::from_raw_parts(self.data.cast::<u64>(), self.buf_bytes() / size_of::<u64>()),
                aux:           core::slice::from_raw_parts(self.aux_data.cast::<u64>(), self.aux_data_u64s()),
                unpack_fn_len: self.unpack_fn_len,
            }
        }
    }

    pub fn keys_mut(&mut self) -> KeysMut<'_> {
        self.buf_aligned();
        let node = self as *const BtreeNode;
        let (buf_u64s, aux_u64s) = (self.buf_bytes() / size_of::<u64>(), self.aux_data_u64s());
        let BtreeNode { format, nr_key_bits, unpack_fn_len, set, nsets, nr, data, aux_data, .. } = self;
        unsafe {
            KeysMut {
                node,
                format,
                nr_key_bits:   *nr_key_bits,
                unpack_fn_len: *unpack_fn_len,
                sets:          Sets { set, nr: nsets },
                nr,
                buf:           core::slice::from_raw_parts_mut(data.cast::<u64>(), buf_u64s),
                aux:           core::slice::from_raw_parts_mut(aux_data.cast::<u64>(), aux_u64s),
            }
        }
    }
}

#[cold]
#[inline(never)]
fn no_key_at(offset: u16, len: usize) -> ! {
    panic!("no key at offset {offset} of a {len} u64 node")
}

/// The u64s of the key at @offset in @buf, as node read validates them: not
/// zero, and all of the key in the buffer.
#[inline(always)]
fn key_u64s_at(buf: &[u64], offset: u16) -> usize {
    let at = offset as usize;
    invariant!(at < buf.len());

    let u64s = buf[at].to_ne_bytes()[0] as usize;
    invariant!(u64s != 0, u64s <= buf.len() - at);
    u64s
}

/// The key at @offset in @buf: a node's keys are validated.
#[inline(always)]
pub(super) fn key_at(buf: &[u64], offset: u16) -> &BkeyP {
    let at = offset as usize;
    let u64s = key_u64s_at(buf, offset);

    match BkeyP::from_slice(&buf[at..at + u64s]) {
        Some(k) => k.cast(),
        None    => no_key_at(offset, buf.len()),
    }
}

/// The key after the one at @k in @buf: its end is an offset, a u16, as node
/// read validates.
#[inline(always)]
pub(super) fn bkey_p_next(buf: &[u64], k: u16) -> u16 {
    let next = k as usize + key_u64s_at(buf, k);
    invariant!(next <= u16::MAX as usize);
    u16::try_from(next).unwrap_or_else(|_| no_key_at(k, buf.len()))
}

/// Set @t's keys in @buf, in order, with their offsets.
pub(super) fn set_keys<'a>(buf: &'a [u64], t: &BsetTree) -> impl Iterator<Item = (u16, &'a BkeyP)> + use<'a> {
    let (mut k, end) = (t.first_offset(), t.end_offset);
    core::iter::from_fn(move || {
        if k == end {
            return None;
        }
        let at = k;
        k = bkey_p_next(buf, k);
        invariant!(k <= end);
        Some((at, key_at(buf, at)))
    })
}

impl<'a> Keys<'a> {
    /// For its format fields: through a KeysMut's view, what that borrows
    /// mutably isn't read through this.
    pub fn node(&self) -> &'a BtreeNode {
        unsafe { &*self.node }
    }

    #[inline(always)]
    pub fn key_at(&self, offset: u16) -> &'a BkeyP {
        key_at(self.buf, offset)
    }

    #[inline(always)]
    pub fn bkey_p_next(&self, k: u16) -> u16 {
        bkey_p_next(self.buf, k)
    }

    pub fn unpack_pos(&self, k: &BkeyP) -> Bpos {
        unsafe { k.unpack_pos_raw(self.node) }
    }

    pub fn key_offset(&self, k: &BkeyP) -> u16 {
        self.offset_of(k.as_ffi())
    }

    /// Of a pointer C has into the buffer.
    pub fn offset_of<T>(&self, p: *const T) -> u16 {
        let offset = ((p as usize).wrapping_sub(self.buf.as_ptr() as usize) / size_of::<u64>()) as u16;

        ebug_assert!(self.buf.as_ptr().wrapping_add(offset as usize).cast() == p,
                     "{p:p}, not at offset {offset} of its node at {:p}", self.buf.as_ptr());
        offset
    }

    /// For C, the key at @offset.
    pub fn ptr_at(&self, offset: u16) -> *mut c::bkey_packed {
        self.buf.as_ptr().wrapping_add(offset as usize).cast_mut().cast()
    }

    /// Set @t's keys, in order, with their offsets.
    pub fn set_keys(&self, t: &BsetTree) -> impl Iterator<Item = (u16, &'a BkeyP)> + use<'a> {
        set_keys(self.buf, t)
    }

    pub fn bset(&self, t: &BsetTree) -> &'a c::bset {
        from_bytes(plain_bytes(&self.buf[t.data_offset as usize..]))
            .unwrap_or_else(|e| panic!("no bset at offset {} of a {} u64 node: {e}", t.data_offset, self.buf.len()))
    }

    pub(super) fn header(&self) -> &'a Head<c::btree_node> {
        cast_plain(plain_bytes(self.buf)).expect("a btree node's buffer holds its header")
    }

    pub fn min_key(&self) -> Bpos {
        self.header().min_key
    }

    pub fn max_key(&self) -> Bpos {
        self.header().max_key
    }
}

impl KeysMut<'_> {
    pub fn as_keys(&self) -> Keys<'_> {
        Keys { node: self.node, sets: &self.sets, buf: self.buf, aux: self.aux, unpack_fn_len: self.unpack_fn_len }
    }

    pub fn key_at_mut(&mut self, offset: u16) -> &mut BkeyP {
        let (at, len) = (offset as usize, self.buf.len());
        let u64s = key_u64s_at(self.buf, offset);

        match BkeyP::from_slice_mut(&mut self.buf[at..at + u64s]) {
            Some(k) => k.cast_mut(),
            None    => no_key_at(offset, len),
        }
    }

    pub fn unpack_pos(&self, k: &BkeyP) -> Bpos {
        unsafe { k.unpack_pos_raw(self.node) }
    }

    /// Set @set's bset is at @data_offset.
    pub fn set_bset(&mut self, set: usize, data_offset: u16) {
        self.sets[set].data_offset = data_offset;
        self.set_bset_end(set);
    }

    /// Set @set's end, from its bset's u64s.
    pub fn set_bset_end(&mut self, set: usize) {
        let t = &self.sets[set];
        let end = t.first_offset() as usize + self.as_keys().bset(t).u64s();

        invariant!(end <= u16::MAX as usize);
        self.sets[set].end_offset = u16::try_from(end)
            .unwrap_or_else(|_| crate::btree::invariant_failed("a bset past the last offset"));
    }
}

impl Sets<'_> {
    /// A new set, at the end: its index.
    pub fn push(&mut self) -> usize {
        let i = *self.nr as usize;
        assert!(i < self.set.len(), "a btree node with {i} bsets, of at most {}", self.set.len());
        *self.nr += 1;
        i
    }

    /// No sets: none with a bset, none with an aux tree.
    pub fn clear(&mut self) {
        *self.nr = 0;
        for t in self.set.iter_mut() {
            t.data_offset = u16::MAX;
        }
        self.set_no_aux_tree(0);
    }

    /// From set @from on, to the last there could be: none has an aux tree.
    pub fn set_no_aux_tree(&mut self, from: usize) {
        for t in &mut self.set[from..] {
            t.size            = 0;
            t.extra           = BSET_NO_AUX_TREE_VAL;
            t.aux_data_offset = u16::MAX;
        }
    }
}

impl Deref for Sets<'_> {
    type Target = [BsetTree];

    fn deref(&self) -> &[BsetTree] {
        invariant!(*self.nr as usize <= self.set.len());
        &self.set[..*self.nr as usize]
    }
}

impl DerefMut for Sets<'_> {
    fn deref_mut(&mut self) -> &mut [BsetTree] {
        &mut self.set[..*self.nr as usize]
    }
}
