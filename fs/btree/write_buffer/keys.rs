// SPDX-License-Identifier: GPL-2.0

use crate::btree::{bkey::{BkeyI, BKEY_U64S}, write_buffer::{BtreeWriteBufferedKey, WbKeyRef}};
use crate::data::keylist;
use crate::data_structures::dst_room::DstRoom;
use crate::util::alloc::{Flags, AllocError, Vec};
use crate::os::kernel::prefetch;
use crate::util::projections::Volatile;
use crate::util::vstructs::{plain_bytes, plain_bytes_mut};
use crate::format::{Bpos, Bkey};
use super::{REF_HI_MI_LO, REF_IDX, REF_POS, SEQ_U64S};
use core::marker::PhantomData;
use core::ops::{Deref, Range};
use core::ptr::NonNull;

/// A key in the write buffer, over its u64s.
type KeyRoom = DstRoom<BtreeWriteBufferedKey>;

/// Its size in the write buffer: the key, and its journal seq.
pub(super) fn wb_key_u64s(k: &Bkey) -> usize {
    usize::from(k.u64s) + SEQ_U64S
}

crate::published! {
    /// A buffer's length and capacity, in u64s, as they're seen without its
    /// lock: by must_wait(), the flush triggers and to_text().
    #[derive(::pin_init::Zeroable)]
    pub struct KeysLive {
        pub nr:   Volatile<usize>,
        pub size: Volatile<usize>,
    }
}

impl KeysLiveWriter {
    pub(super) fn set(&self, nr: usize, size: usize) {
        self.nr.set(nr);
        self.size.set(size);
    }
}

/// A write buffer's keys, as the journal intake stages them: each a
/// BtreeWriteBufferedKey, its journal seq then the key, as many u64s as
/// the key has. A seq of 0 is a key flushed, or dropped as a duplicate.
///
/// What changes its length or capacity takes the KeysLive its lock
/// publishes them in, and sets it.
#[derive(Default)]
pub struct WbKeys(Vec<u64>);

impl Deref for WbKeys {
    type Target = Vec<u64>;

    fn deref(&self) -> &Vec<u64> {
        &self.0
    }
}

impl WbKeys {
    pub(super) fn room(&self) -> usize {
        self.capacity() - self.len()
    }

    fn publish(&self, live: &KeysLiveWriter) {
        live.set(self.len(), self.capacity());
    }

    pub(super) fn resize(&mut self, live: &KeysLiveWriter, new_size: usize, gfp: Flags)
        -> Result<(), AllocError>
    {
        let ret = super::resize_gfp(&mut self.0, new_size, gfp);
        self.publish(live);
        ret
    }

    pub(super) fn clear(&mut self, live: &KeysLiveWriter) {
        self.0.clear();
        self.publish(live);
    }

    /// @self's keys and @other's traded, buffers and all.
    pub(super) fn swap(&mut self, live: &KeysLiveWriter, other: &mut WbKeys, other_live: &KeysLiveWriter) {
        core::mem::swap(self, other);
        self.publish(live);
        other.publish(other_live);
    }

    pub(super) fn seq(&self, idx: usize) -> u64 {
        self.0[idx]
    }

    pub(super) fn key(&self, idx: usize) -> &BtreeWriteBufferedKey {
        KeyRoom::from_elems(&self.0[idx..self.next(idx)])
    }

    fn next(&self, idx: usize) -> usize {
        let k: &BkeyI = keylist::keys(&self.0[idx + SEQ_U64S..]).next().expect("a key at a write buffer key's index");
        idx + wb_key_u64s(&k.k)
    }

    /// The u64 index of each key.
    pub(super) fn idxs(&self) -> impl Iterator<Item = usize> + '_ {
        core::iter::successors((!self.is_empty()).then_some(0), |&i| {
            Some(self.next(i)).filter(|&n| n < self.len())
        })
    }

    pub(super) fn get_mut(&mut self, idx: usize) -> &mut BtreeWriteBufferedKey {
        let end = self.next(idx);
        KeyRoom::from_elems_mut(&mut self.0[idx..end])
    }

    /// The keys at @a and @b, which are different keys.
    pub(super) fn pair_mut(&mut self, a: usize, b: usize)
        -> (&mut BtreeWriteBufferedKey, &mut BtreeWriteBufferedKey)
    {
        let (a_end, b_end) = (self.next(a), self.next(b));
        assert!(a_end <= b || b_end <= a, "write buffer keys at {a} and {b} overlap");

        let (a, b) = if a < b {
            let (l, r) = self.0.split_at_mut(b);
            (&mut l[a..a_end], &mut r[..b_end - b])
        } else {
            let (l, r) = self.0.split_at_mut(a);
            (&mut r[..a_end - a], &mut l[b..b_end])
        };
        (KeyRoom::from_elems_mut(a), KeyRoom::from_elems_mut(b))
    }

    /// @k, from journal entry @seq, in room the caller checked.
    pub(super) fn push(&mut self, live: &KeysLiveWriter, seq: u64, k: &BkeyI) {
        assert!(self.room() >= wb_key_u64s(&k.k), "a write buffer key pushed without room");

        let _ = self.0.push_within_capacity(seq);
        let dst = self.0.extend_zeroed_within_capacity(usize::from(k.k.u64s)).expect("room checked");
        plain_bytes_mut(dst).copy_from_slice(plain_bytes(k.as_bytes()));
        self.publish(live);
    }

    /// All of @src's keys added, in room the caller checked; @src left empty.
    pub(super) fn append(&mut self, live: &KeysLiveWriter, src: &mut WbKeys, src_live: &KeysLiveWriter) {
        self.0.extend_from_slice(&src.0).expect("room checked");
        src.clear(src_live);
        self.publish(live);
    }

    /// As many of @src's keys as fit in @room u64s, in all, moved to the end
    /// of these: whole keys, from the front.
    pub(super) fn move_from(&mut self, live: &KeysLiveWriter, src: &mut WbKeys, src_live: &KeysLiveWriter,
                            room: usize) {
        let mut moved = 0;
        while moved < src.len() {
            let next = src.next(moved);
            if self.len() + (next - moved) > room {
                break;
            }
            moved = next;
        }

        self.0.extend_from_slice(&src.0[..moved]).expect("room checked");
        src.0.copy_within(moved.., 0);
        src.0.truncate(src.len() - moved);
        self.publish(live);
        src.publish(src_live);
    }

    /// The keys not yet flushed - with a seq - moved down, the rest dropped.
    pub(super) fn retain_unflushed(&mut self, live: &KeysLiveWriter) {
        let (mut i, mut dst) = (0, 0);

        while i < self.len() {
            let next = self.next(i);
            if self.seq(i) != 0 {
                self.0.copy_within(i..next, dst);
                dst += next - i;
            }
            i = next;
        }
        self.0.truncate(dst);
        self.publish(live);
    }
}

/// A slice of a flush's sorted refs, and the flushing keys they refer to.
/// split() hands each shard its own slice of the refs, and the refs are
/// distinct, one for each key: so a key is reached by one shard, and that
/// shard has it one at a time.
pub(super) struct Shard<'a> {
    refs:  &'a [WbKeyRef],
    range: Range<usize>,
    keys:  NonNull<u64>,
    len:   usize,
    _keys: PhantomData<&'a mut WbKeys>,
}

// Each shard has keys no other shard has: new()'s.
unsafe impl Send for Shard<'_> {}

impl<'a> Shard<'a> {
    /// All of @refs.
    ///
    /// # Safety
    /// @refs are distinct, each the index of a key in @keys.
    pub(super) unsafe fn new(keys: &'a mut WbKeys, refs: &'a [WbKeyRef]) -> Self {
        let len = keys.len();
        let keys = NonNull::new(keys.0.as_mut_ptr()).expect("a vec's pointer");
        Shard { refs, range: 0..refs.len(), keys, len, _keys: PhantomData }
    }

    /// @n shards, each of an even share of these refs, in order.
    pub(super) fn split(&mut self, n: usize) -> impl Iterator<Item = Shard<'_>> {
        let (refs, keys, len, start, nr) = (self.refs, self.keys, self.len, self.range.start, self.range.len());
        (0..n).map(move |i| {
            let range = start + (nr * i) / n..start + (nr * (i + 1)) / n;
            Shard { refs, range, keys, len, _keys: PhantomData }
        })
    }

    pub(super) fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    pub(super) fn idx(&self, i: usize) -> usize {
        self.refs[i].idx()
    }

    pub(super) fn prefetch(&self, i: usize) {
        prefetch(self.keys.as_ptr().wrapping_add(self.idx(i)));
    }

    /// The key of the shard's @i'th ref.
    pub(super) fn key(&mut self, i: usize) -> &mut BtreeWriteBufferedKey {
        assert!(self.range.contains(&i));
        let idx = self.idx(i);
        if idx + BKEY_U64S > self.len {
            panic!("at {}/{} of wb->sorted got idx {}/{}", i, self.refs.len(), idx, self.len);
        }

        let k = self.keys.as_ptr().wrapping_add(idx);
        let u64s = SEQ_U64S + usize::from(unsafe { (*k.add(SEQ_U64S).cast::<Bkey>()).u64s });
        assert!(idx + u64s <= self.len, "write buffer key at {idx} of {} u64s overruns the {}", u64s, self.len);

        KeyRoom::from_elems_mut(unsafe { core::slice::from_raw_parts_mut(k, u64s) })
    }
}

impl WbKeyRef {
    /// The ref of the key at @idx, at @pos.
    pub(super) fn new(idx: usize, pos: Bpos) -> Self {
        let mut r = WbKeyRef { words: [0; 3] };
        let b = plain_bytes_mut(&mut r.words[..]);
        b[REF_IDX].copy_from_slice(&u32::try_from(idx).expect("a write buffer key's index, in 32 bits").to_ne_bytes());
        b[REF_POS].copy_from_slice(plain_bytes(&pos));
        r
    }

    pub(super) fn idx(&self) -> usize {
        u32::from_ne_bytes(plain_bytes(&self.words[..])[REF_IDX].try_into().expect("4 bytes")) as usize
    }

    pub(super) fn words(&self) -> (u64, u64, u64) {
        let [hi, mi, lo] = REF_HI_MI_LO.map(|i| self.words[i]);
        (hi, mi, lo)
    }
}
