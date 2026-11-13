// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::{BkeyI, BKEY_U64S};
use crate::c;
use crate::data::keylist;
use crate::util::alloc::{AllocError, KVVec};
use crate::os::kernel::prefetch;
use crate::util::projections::Volatile;
use crate::util::vstructs::{plain_bytes, plain_bytes_mut};
use core::marker::PhantomData;
use core::mem::{offset_of, size_of};
use core::ops::{Deref, Range};
use core::ptr::NonNull;

const SEQ_U64S: usize = offset_of!(c::btree_write_buffered_key, k) / size_of::<u64>();
const _: () = assert!(offset_of!(c::btree_write_buffered_key, journal_seq) == 0 && SEQ_U64S == 1);

/// Its size in the write buffer: the key, and its journal seq.
pub(super) fn wb_key_u64s(k: &c::bkey) -> usize {
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

/// A write buffer's keys, as the journal intake stages them: each its
/// journal seq, then the key - as many u64s as the key has, not a whole
/// btree_write_buffered_key, whose padding may run past the end. A seq of 0
/// is a key flushed, or dropped as a duplicate.
///
/// What changes its length or capacity takes the KeysLive its lock
/// publishes them in, and sets it.
#[derive(Default)]
pub struct WbKeys(KVVec<u64>);

impl Deref for WbKeys {
    type Target = KVVec<u64>;

    fn deref(&self) -> &KVVec<u64> {
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

    pub(super) fn resize(&mut self, live: &KeysLiveWriter, new_size: usize, gfp: c::gfp_t)
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

    pub(super) fn key(&self, idx: usize) -> &BkeyI {
        keylist::keys(&self.0[idx + SEQ_U64S..]).next().expect("a key at a write buffer key's index")
    }

    fn next(&self, idx: usize) -> usize {
        idx + wb_key_u64s(&self.key(idx).k)
    }

    /// The u64 index of each key.
    pub(super) fn idxs(&self) -> impl Iterator<Item = usize> + '_ {
        core::iter::successors((!self.is_empty()).then_some(0), |&i| {
            Some(self.next(i)).filter(|&n| n < self.len())
        })
    }

    pub(super) fn get_mut(&mut self, idx: usize) -> WbKeyMut<'_> {
        let end = self.next(idx);
        WbKeyMut(&mut self.0[idx..end])
    }

    /// The keys at @a and @b, which are different keys.
    pub(super) fn pair_mut(&mut self, a: usize, b: usize) -> (WbKeyMut<'_>, WbKeyMut<'_>) {
        let (a_end, b_end) = (self.next(a), self.next(b));
        assert!(a_end <= b || b_end <= a, "write buffer keys at {a} and {b} overlap");

        if a < b {
            let (l, r) = self.0.split_at_mut(b);
            (WbKeyMut(&mut l[a..a_end]), WbKeyMut(&mut r[..b_end - b]))
        } else {
            let (l, r) = self.0.split_at_mut(a);
            (WbKeyMut(&mut r[..a_end - a]), WbKeyMut(&mut l[b..b_end]))
        }
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

/// A write buffer key had mutably: its u64s, seq then key.
pub(super) struct WbKeyMut<'a>(&'a mut [u64]);

impl WbKeyMut<'_> {
    pub(super) fn seq(&self) -> u64 {
        self.0[0]
    }

    pub(super) fn set_seq(&mut self, seq: u64) {
        self.0[0] = seq
    }

    pub(super) fn k(&self) -> &BkeyI {
        keylist::keys(&self.0[SEQ_U64S..]).next().expect("a key in its u64s")
    }

    pub(super) fn k_mut(&mut self) -> &mut BkeyI {
        keylist::key_mut(&mut self.0[SEQ_U64S..])
    }
}

/// A slice of a flush's sorted refs, and the flushing keys they refer to.
/// split() hands each shard its own slice of the refs, and the refs are
/// distinct, one for each key: so a key is reached by one shard, and that
/// shard has it one at a time.
pub(super) struct Shard<'a> {
    refs:  &'a [c::wb_key_ref],
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
    pub(super) unsafe fn new(keys: &'a mut WbKeys, refs: &'a [c::wb_key_ref]) -> Self {
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
    pub(super) fn key(&mut self, i: usize) -> WbKeyMut<'_> {
        assert!(self.range.contains(&i));
        let idx = self.idx(i);
        if idx + BKEY_U64S > self.len {
            panic!("at {}/{} of wb->sorted got idx {}/{}", i, self.refs.len(), idx, self.len);
        }

        let k = self.keys.as_ptr().wrapping_add(idx);
        let u64s = SEQ_U64S + usize::from(unsafe { (*k.add(SEQ_U64S).cast::<c::bkey>()).u64s });
        assert!(idx + u64s <= self.len, "write buffer key at {idx} of {} u64s overruns the {}", u64s, self.len);

        WbKeyMut(unsafe { core::slice::from_raw_parts_mut(k, u64s) })
    }
}

impl c::wb_key_ref {
    pub(super) fn idx(&self) -> usize {
        unsafe { self.key.pos_idx.idx as usize }
    }

    pub(super) fn words(&self) -> (u64, u64, u64) {
        let w = unsafe { self.key.words };
        (w.hi, w.mi, w.lo)
    }
}
