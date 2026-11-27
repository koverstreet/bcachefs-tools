// SPDX-License-Identifier: GPL-2.0

//! A node's keys, its buffers as slices: the node holds thin pointers, and
//! a view is made of them once per operation, not per access. Keys and bsets
//! are referred to, as in C, by their offsets in u64s from the node's start.

use crate::btree::bkey::{BkeyP, BkeySC};
use crate::btree::node::bset::BsetTree;
use crate::btree::node::{BtreeNode, NodeContents};
use crate::format::{btree_node, Bkey, Bpos, Bset};
use crate::types::L1_CACHE_BYTES;
use crate::util::vstructs::{cast_plain, plain_bytes, Head};

pub struct Keys<'a> {
    node:              &'a BtreeNode,
    contents:          &'a NodeContents,
    pub sets:          &'a [BsetTree],
    /// The node's buffer, 1 << byte_order bytes: its bsets, and the
    /// unwritten whiteouts at its end.
    pub buf:           &'a [u64],
    /// The bsets' lookup tables, each at its set's aux_data_offset.
    pub aux:           &'a [u64],
    pub unpack_fn_len: u8,
}

/// Keys, to change: the contents, and the buffers they point to.
pub struct KeysMut<'a> {
    pub(super) node: &'a BtreeNode,
    pub contents:    &'a mut NodeContents,
    pub buf:         &'a mut [u64],
    pub aux:         &'a mut [u64],
}

impl BtreeNode {
    /// The aux trees' cachelines are the buffer's, by offset: rounding an
    /// offset is rounding the address only if the buffer is L1-aligned.
    fn buf_aligned(data: *const Head<btree_node>) {
        ebug_assert!(data as usize % L1_CACHE_BYTES == 0,
                     "btree node buffer at {data:p}, not {L1_CACHE_BYTES} byte aligned");
    }

    pub(super) fn own_contents(&self, contents: *const NodeContents) {
        ebug_assert!(core::ptr::eq(contents, self.contents.data_ptr()), "another node's contents");
    }

    #[inline]
    pub fn keys<'a>(&'a self, contents: &'a NodeContents) -> Keys<'a> {
        self.own_contents(contents);
        Self::buf_aligned(contents.bufs.data.cast());
        unsafe {
            Keys {
                node:          self,
                contents,
                sets:          contents.sets(),
                buf:           core::slice::from_raw_parts(contents.bufs.data, self.buf_bytes() / size_of::<u64>()),
                aux:           core::slice::from_raw_parts(contents.bufs.aux_data, self.aux_data_u64s()),
                unpack_fn_len: contents.unpack_fn_len,
            }
        }
    }

    /// The buffer and the aux trees are the contents', had mutably with them.
    pub fn keys_mut<'a>(&'a self, contents: &'a mut NodeContents) -> KeysMut<'a> {
        self.own_contents(contents);
        Self::buf_aligned(contents.bufs.data.cast());
        let (buf_u64s, aux_u64s) = (self.buf_bytes() / size_of::<u64>(), self.aux_data_u64s());
        let (data, aux_data) = (contents.bufs.data, contents.bufs.aux_data);
        unsafe {
            KeysMut {
                node: self,
                contents,
                buf:  core::slice::from_raw_parts_mut(data, buf_u64s),
                aux:  core::slice::from_raw_parts_mut(aux_data, aux_u64s),
            }
        }
    }
}

#[cold]
#[inline(never)]
fn no_key_at(offset: usize, len: usize) -> ! {
    panic!("no key at offset {offset} of a {len} u64 node")
}

/// The u64s of the key at @at in @buf, as node read validates them: not
/// zero, and all of the key in the buffer.
#[inline(always)]
fn key_u64s_at(buf: &[u64], at: usize) -> usize {
    invariant!(at < buf.len());

    let u64s = buf[at].to_ne_bytes()[0] as usize;
    invariant!(u64s != 0, u64s <= buf.len() - at);
    u64s
}

/// The key at @offset in @buf: a node's keys are validated. A usize for the
/// unwritten whiteouts, which run to the buffer's end.
#[inline(always)]
pub(super) fn key_at(buf: &[u64], offset: impl Into<usize>) -> &BkeyP {
    let at = offset.into();
    let u64s = key_u64s_at(buf, at);

    match BkeyP::from_slice(&buf[at..][..u64s]) {
        Some(k) => k.cast(),
        None    => no_key_at(at, buf.len()),
    }
}

/// The key after the one at @k in @buf: its end is an offset, a u16, as node
/// read validates.
#[inline(always)]
pub(super) fn bkey_p_next(buf: &[u64], k: u16) -> u16 {
    let next = k as usize + key_u64s_at(buf, k.into());
    invariant!(next <= u16::MAX as usize);
    u16::try_from(next).unwrap_or_else(|_| no_key_at(k.into(), buf.len()))
}

/// The keys in @buf from @k to @end, in order, with their offsets.
#[inline(always)]
pub(super) fn keys_in(buf: &[u64], mut k: usize, end: usize) -> impl Iterator<Item = (usize, &BkeyP)> {
    core::iter::from_fn(move || {
        if k == end {
            return None;
        }
        let (at, key) = (k, key_at(buf, k));
        k += key.u64s();
        invariant!(k <= end);
        Some((at, key))
    })
}

/// keys_in(), mutably.
fn keys_in_mut(buf: &mut [u64], mut k: usize, end: usize) -> impl Iterator<Item = &mut BkeyP> {
    let mut rest = &mut buf[k..end];
    core::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (at, u64s) = (k, key_u64s_at(rest, 0));
        let (key, tail) = core::mem::take(&mut rest).split_at_mut(u64s);
        rest = tail;
        k += u64s;
        Some(BkeyP::from_slice_mut(key).unwrap_or_else(|| no_key_at(at, end)).cast_mut())
    })
}

/// keys_in(), between a node's offsets.
#[inline(always)]
pub(super) fn keys_between(buf: &[u64], k: u16, end: u16) -> impl Iterator<Item = (u16, &BkeyP)> {
    keys_in(buf, k.into(), end.into()).map(|(at, k)| (at as u16, k))
}

/// Set @t's keys in @buf, in order, with their offsets.
pub(super) fn set_keys<'a>(buf: &'a [u64], t: &BsetTree) -> impl Iterator<Item = (u16, &'a BkeyP)> + use<'a> {
    keys_between(buf, t.first_offset(), t.end_offset)
}

impl<'a> Keys<'a> {
    pub fn node(&self) -> &'a BtreeNode {
        self.node
    }

    pub fn contents(&self) -> &'a NodeContents {
        self.contents
    }

    #[inline(always)]
    pub fn key_at(&self, offset: u16) -> &'a BkeyP {
        key_at(self.buf, offset)
    }

    #[inline(always)]
    pub fn bkey_p_next(&self, k: u16) -> u16 {
        bkey_p_next(self.buf, k)
    }

    pub fn unpack_pos(&self, k: u16) -> Bpos {
        self.contents().unpack_pos_at(self.buf, k)
    }

    /// Just the key at @k, not the value.
    pub fn unpack(&self, k: u16) -> Bkey {
        self.contents().unpack_key_at(self.buf, k)
    }

    /// The key at @k unpacked into @u, the value where it is.
    pub fn disassemble<'u>(&self, k: u16, u: &'u mut Bkey) -> BkeySC<'u> where 'a: 'u {
        self.key_at(k).disassembled(&self.contents().format, self.unpack(k), u)
    }

    pub fn key_offset(&self, k: &BkeyP) -> u16 {
        self.offset_of(k.as_slice().as_ptr())
    }

    /// Of a pointer C has into the buffer.
    pub fn offset_of<T>(&self, p: *const T) -> u16 {
        let offset = ((p as usize).wrapping_sub(self.buf.as_ptr() as usize) / size_of::<u64>()) as u16;

        ebug_assert!(self.buf.as_ptr().wrapping_add(offset as usize).cast() == p,
                     "{p:p}, not at offset {offset} of its node at {:p}", self.buf.as_ptr());
        offset
    }

    #[inline(always)]
    pub fn keys_between(&self, k: u16, end: u16) -> impl Iterator<Item = (u16, &'a BkeyP)> + use<'a> {
        keys_between(self.buf, k, end)
    }

    /// Set @t's keys, in order, with their offsets.
    pub fn set_keys(&self, t: &BsetTree) -> impl Iterator<Item = (u16, &'a BkeyP)> + use<'a> {
        set_keys(self.buf, t)
    }

    /// The unwritten whiteouts, at the buffer's end, with their offsets: past
    /// a 512k node's last u16, so not a u16's.
    pub fn unwritten_whiteouts(&self) -> impl Iterator<Item = (usize, &'a BkeyP)> + use<'a> {
        let r = self.node().unwritten_whiteouts();
        keys_in(self.buf, r.start, r.end)
    }

    pub fn bset(&self, t: &BsetTree) -> &'a Head<Bset> {
        cast_plain(plain_bytes(&self.buf[t.data_offset as usize..]))
            .unwrap_or_else(|| panic!("no bset at offset {} of a {} u64 node", t.data_offset, self.buf.len()))
    }

    pub(super) fn header(&self) -> &'a Head<btree_node> {
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
        let contents = &*self.contents;
        Keys { node: self.node, contents, sets: contents.sets(), buf: self.buf, aux: self.aux,
               unpack_fn_len: contents.unpack_fn_len }
    }

    /// Every set's keys, set by set, mutably: the sets are in buffer order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut BkeyP> {
        let (mut rest, mut base) = (&mut *self.buf, 0);

        self.contents.sets().iter().flat_map(move |t| {
            let (first, end) = (t.first_offset() as usize, t.end_offset as usize);
            let (set, tail) = core::mem::take(&mut rest)[first - base..].split_at_mut(end - first);

            (rest, base) = (tail, end);
            keys_in_mut(set, 0, set.len())
        })
    }

    pub fn key_at_mut(&mut self, offset: u16) -> &mut BkeyP {
        let (at, len) = (offset as usize, self.buf.len());
        let u64s = key_u64s_at(self.buf, offset.into());

        match BkeyP::from_slice_mut(&mut self.buf[at..][..u64s]) {
            Some(k) => k.cast_mut(),
            None    => no_key_at(offset.into(), len),
        }
    }

    /// Set @set's bset is at @data_offset.
    pub fn set_bset(&mut self, set: usize, data_offset: u16) {
        self.contents.sets_mut()[set].data_offset = data_offset;
        self.set_bset_end(set);
    }

    /// Set @set's end, from its bset's u64s.
    pub fn set_bset_end(&mut self, set: usize) {
        let t = &self.contents.sets()[set];
        let end = t.first_offset() as usize + self.as_keys().bset(t).u64s();

        invariant!(end <= u16::MAX as usize);
        self.contents.sets_mut()[set].end_offset = u16::try_from(end)
            .unwrap_or_else(|_| crate::invariant_failed("a bset past the last offset"));
    }
}

