// SPDX-License-Identifier: GPL-2.0

//! Keylists: whole keys back to back, added at the top and taken off the
//! front, the rest moving down.
//!
//! Keylist is an InlineVec of u64s: a write op's, in the op until its keys
//! outgrow it, and an interior update's parent keys, which never do.
//!
//! What's trusted is that the u64s hold whole keys, each made in memory by
//! what added it: the keys are walked as vstructs, each one's u64s checked
//! against what's left of the list, and taken as validated - add() taking a
//! validated key would make that by type. A key is built in a room, a
//! DstRoom - at the top, in room reserve() made, by push_with() - which checks
//! what's appended to it against the room.

use crate::btree::bkey::{AnyKey, BkeyI, BkeySC, BkeyV, BKEY_U64S};
use crate::btree::iter::NoRoom;
use crate::c;
use crate::data_structures::dst_room::{DstRoom, Storage};
use crate::data_structures::inline_vec::InlineVec;
use crate::locking::MemallocFlags;
use crate::util::alloc::{Alloc, AllocError, Box};
use crate::util::vstructs::{plain_bytes, plain_bytes_mut, SelfSized, Unsized, Unvalidated, UnvalidatedEntries, UnvalidatedEntriesMut};
use core::mem::size_of;


/// Whole keys back to back, the first @N u64s of them inline.
#[repr(transparent)]
pub struct Keylist<const N: usize>(InlineVec<u64, N>);

impl<const N: usize> Keylist<N> {
    pub const fn new() -> Self {
        Keylist(InlineVec::new())
    }

    pub fn u64s(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear()
    }

    /// Room for @u64s more: allocated noio, as it's the write path's.
    pub fn reserve(&mut self, u64s: usize) -> Result<(), AllocError> {
        let _noio = MemallocFlags::noio();
        self.0.reserve(u64s)
    }

    /// A key built by @f at the top, in @room u64s of what reserve() made,
    /// added. Panics if the room wasn't reserved.
    pub fn push_with<R>(&mut self, room: usize, f: impl FnOnce(&mut DstRoom<BkeyI>) -> R) -> R {
        let (len, cap) = (self.0.len(), self.0.capacity());
        assert!(len + room <= cap, "a key built in {room} u64s of a keylist with {} reserved", cap - len);
        for _ in 0..room {
            self.0.push(0).expect("reserved");
        }

        let k = DstRoom::<BkeyI>::new_in(&mut self.0[len..]);
        let r = f(k);
        let u64s = usize::from(k.k.u64s);
        self.0.truncate(len + u64s);
        r
    }

    /// A copy of @k added, in room reserve() made.
    pub fn push_key(&mut self, k: &BkeyI) {
        self.push_with(k.k().u64s.into(), |r| r.reassemble(k.into()).expect("room for its u64s"))
    }

    pub fn iter(&self) -> impl Iterator<Item = &BkeyI> + '_ {
        self.iter_from(0)
    }

    /// The keys from @offset u64s in: where a key starts.
    pub fn iter_from(&self, offset: usize) -> impl Iterator<Item = &BkeyI> + '_ {
        keys(&self.0[offset..])
    }

    pub fn front(&self) -> Option<&BkeyI> {
        self.iter().next()
    }

    pub fn front_mut(&mut self) -> Option<&mut BkeyI> {
        (!self.is_empty()).then(|| key_mut(&mut self.0))
    }

    /// @f on each key, which may shrink it, the keys after it moving down -
    /// stopping at @f's error, which leaves the list partly moved.
    pub fn try_for_each_shrink<E>(&mut self, mut f: impl FnMut(&mut BkeyI) -> Result<(), E>) -> Result<(), E> {
        let (mut src, mut dst) = (0, 0);

        while src < self.0.len() {
            let k = key_mut(&mut self.0[src..]);
            let n = usize::from(k.k().u64s);
            f(&mut *k)?;
            let shrunk = usize::from(k.k().u64s);
            assert!(shrunk <= n, "a key grown in its keylist, {n} u64s to {shrunk}");

            self.0.copy_within(src..src + shrunk, dst);
            dst += shrunk;
            src += n;
        }

        self.0.truncate(dst);
        Ok(())
    }

    /// The rest of the keys move down.
    pub fn pop_front(&mut self) {
        let n = usize::from(self.front().expect("popping the front of an empty keylist").k().u64s);
        self.drop_front(n);
    }

    /// The first @u64s taken off the front - whole keys - and the rest moved
    /// down.
    pub fn drop_front(&mut self, u64s: usize) {
        let len = self.u64s();
        assert!(u64s <= len, "dropping {u64s} u64s from a keylist of {len}");

        self.0.copy_within(u64s.., 0);
        self.0.truncate(len - u64s);
    }

    pub fn as_u64s(&self) -> &[u64] {
        &self.0
    }

    pub fn as_u64s_mut(&mut self) -> &mut [u64] {
        &mut self.0
    }

    /// Its keys in increasing order - debug builds'.
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub fn verify_sorted(&self) {
        let mut keys = self.iter().peekable();
        while let Some(k) = keys.next() {
            if let Some(next) = keys.peek() {
                assert!(k.k().p < next.k().p, "keylist out of order: {} before {}", k.k(), next.k());
            }
        }
    }
}

/// A key built in place, in a room: a keylist's top, a builder's own buffer,
/// or a key kept in a struct - C's "struct bkey_i key; u64 key_pad[N];", a
/// DstRoom<BkeyI, [u64; N]>, which zeroed is no key, its u64s 0, short of
/// its own header, until one's written there. What grows it is checked
/// against the room.
impl<V: ?Sized + Unsized> DstRoom<BkeyI<V>>
where
    BkeyI<V>: SelfSized,
{
    /// A deleted key at the front of @buf, as bkey_init() leaves one.
    pub fn new_in(buf: &mut [u64]) -> &mut Self {
        Self::from_elems_mut(buf).init()
    }

    /// A deleted key, in a room of @u64s from @alloc.
    pub fn new_boxed_in<A: Alloc>(u64s: usize, alloc: A) -> Result<Box<Self, A>, AllocError> {
        let b = Box::<[u64], A>::try_zeroed_slice_in(u64s, alloc)?;
        let mut b = unsafe { Box::cast_unsized(b, Self::from_elems_raw) };
        b.init();
        Ok(b)
    }
}

impl<V: ?Sized + Unsized, B: ?Sized + Storage<Elem = u64>> DstRoom<BkeyI<V>, B>
where
    BkeyI<V>: SelfSized,
{
    /// A deleted key, as bkey_init() leaves one.
    pub fn init(&mut self) -> &mut Self {
        let have_u64s = self.elems().len();
        assert!(have_u64s >= BKEY_U64S, "room of {have_u64s} u64s, short of a key's header");
        plain_bytes_mut(&mut self.elems_mut()[..BKEY_U64S]).copy_from_slice(plain_bytes(&c::bkey::new()));
        self
    }

    /// Its value, as long as its u64s say.
    pub fn val_mut(&mut self) -> &mut [u64] {
        let u64s = usize::from(self.k.u64s);
        &mut self.elems_mut()[BKEY_U64S..u64s]
    }

    /// Its value made @val_u64s long; grown, the new u64s are zeroes. NoRoom
    /// past the room, or what a key's u8 of u64s holds.
    pub fn set_val_u64s(&mut self, val_u64s: usize) -> Result<(), NoRoom> {
        let need_u64s = BKEY_U64S + val_u64s;
        let have_u64s = self.grow(need_u64s).len().min(u8::MAX.into());
        if need_u64s > have_u64s {
            return Err(NoRoom { need_u64s, have_u64s });
        }

        let u64s = usize::from(self.k.u64s);
        if need_u64s > u64s {
            self.elems_mut()[u64s..need_u64s].fill(0);
        }
        self.k.u64s = need_u64s as u8;
        Ok(())
    }

    /// @words appended to its value.
    pub fn append_val(&mut self, words: &[u64]) -> Result<(), NoRoom> {
        let n = self.val_mut().len();
        self.set_val_u64s(n + words.len())?;
        self.val_mut()[n..].copy_from_slice(words);
        Ok(())
    }

    /// A copy of @k, header and value.
    pub fn reassemble(&mut self, k: BkeySC<'_>) -> Result<(), NoRoom> {
        let val = k.val_bytes();
        self.set_val_u64s(val.len() / size_of::<u64>())?;
        self.k = *k.k;
        plain_bytes_mut(self.val_mut()).copy_from_slice(val);
        Ok(())
    }

    /// @k copied in, from any form of key, panicking if it doesn't fit: the
    /// generic part reads its header and value, so each instance is that and
    /// a call.
    pub fn set_key(&mut self, k: impl BkeyV) {
        let k = k.as_sc();
        key_set(self.grow(k.k.u64s.into()), k.k, k.val_bytes())
    }
}


/// A key - header @k, value @val - copied into @room: one copy of the code for
/// every key type and room size.
#[inline(never)]
fn key_set(room: &mut [u64], k: &c::bkey, val: &[u8]) {
    let u64s = usize::from(k.u64s);
    assert!(u64s <= room.len(), "a key of {u64s} u64s in a room of {}", room.len());
    crate::btree::bkey_buf::copy_key(room, k, val)
}

/// The keys in @u64s, from where a key starts.
pub(crate) fn keys(u64s: &[u64]) -> impl Iterator<Item = &BkeyI> + '_ {
    let len = u64s.len();
    UnvalidatedEntries::<AnyKey<Unvalidated>>::new(plain_bytes(u64s)).map(move |k| {
        let k = k.unwrap_or_else(|e| panic!("a key overruns its buffer, of {len} u64s: {e}"));
        // SAFETY: a keylist's keys are made in memory, by what adds them
        unsafe { &*BkeyI::from_raw(core::ptr::from_ref(k).cast()) }
    })
}

pub(crate) fn keys_mut(u64s: &mut [u64]) -> impl Iterator<Item = &mut BkeyI> + '_ {
    let len = u64s.len();
    UnvalidatedEntriesMut::<AnyKey<Unvalidated>>::new(plain_bytes_mut(u64s)).map(move |k| {
        let k = k.unwrap_or_else(|e| panic!("a key overruns its buffer, of {len} u64s: {e}"));
        unsafe { &mut *BkeyI::from_raw_mut(core::ptr::from_mut(k).cast()) }
    })
}

/// The key at the front of @u64s.
pub(crate) fn key_mut(u64s: &mut [u64]) -> &mut BkeyI {
    keys_mut(u64s).next().expect("a key in an empty buffer")
}
