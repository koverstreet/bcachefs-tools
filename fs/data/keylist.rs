// SPDX-License-Identifier: GPL-2.0

//! Keylists: whole keys back to back, added at the top and taken off the
//! front, the rest moving down.
//!
//! Keylist is a write op's: an InlineVec of u64s, in the op until its keys
//! outgrow it. C's struct keylist - keys..top, pointing into a buffer of its
//! owner's - is a btree split's parent keys, keylist.h's inlines keeping it;
//! Rust only checks one is sorted.
//!
//! What's trusted is that the u64s hold whole keys, each made in memory by
//! what added it: the keys are walked as vstructs, each one's u64s checked
//! against what's left of the list, and taken as validated - add() taking a
//! validated key would make that by type. A key is built in a KeyRoom - at
//! the top, in room reserve() made, by push_with() - which checks what's
//! appended to it against the room.

#![allow(non_camel_case_types)]

use crate::btree::bkey::{BkeyI, BkeySC, BkeyV, BKEY_U64S};
use crate::btree::iter::NoRoom;
use crate::c;
use crate::data_structures::inline_vec::InlineVec;
use crate::locking::MemallocFlags;
use crate::util::alloc::AllocError;
use crate::util::vstructs::{assume_validated, plain_bytes, plain_bytes_mut, VstructRefEntries, VstructRefEntriesMut};
use core::mem::size_of;
use cstruct_macros::CStruct;
use crate::types::c_default;

/// keys..top, whole keys back to back: C reads each pointer as a bkey_i * or
/// a u64 *, Rust as the u64 *.
#[derive(CStruct)]
#[repr(C)]
pub struct keylist {
    #[c_anon("union { struct bkey_i *keys; u64 *keys_p; }")]
    pub(crate) keys_p: *mut u64,
    #[c_anon("union { struct bkey_i *top; u64 *top_p; }")]
    pub(crate) top_p: *mut u64,
}
c_default!(keylist);

#[cfg(CONFIG_BCACHEFS_DEBUG)]
impl c::keylist {
    fn as_u64s(&self) -> &[u64] {
        if self.top_p == self.keys_p {
            return &[];
        }
        let u64s = (self.top_p.addr() - self.keys_p.addr()) / size_of::<u64>();
        // SAFETY: the keylist's invariant
        unsafe { core::slice::from_raw_parts(self.keys_p, u64s) }
    }
}

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
    pub fn push_with<R>(&mut self, room: usize, f: impl FnOnce(&mut KeyRoom<'_>) -> R) -> R {
        let (len, cap) = (self.0.len(), self.0.capacity());
        assert!(len + room <= cap, "a key built in {room} u64s of a keylist with {} reserved", cap - len);
        for _ in 0..room {
            self.0.push(0).expect("reserved");
        }

        let mut k = KeyRoom::new(&mut self.0[len..]);
        let r = f(&mut k);
        let u64s = usize::from(k.k().u64s);
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
        let u64s = self.u64s();

        self.0.copy_within(n.., 0);
        self.0.truncate(u64s - n);
    }
}

/// A key built in place: at the front of a buffer, the rest of it room its
/// value can grow into - a keylist's top, or a builder's own buffer. What
/// grows it is checked against the room, and every view of it, its u64s.
pub struct KeyRoom<'b> {
    buf: &'b mut [u64],
}

impl<'b> KeyRoom<'b> {
    /// A deleted key at the front of @buf, as bkey_init() leaves one.
    pub fn new(buf: &'b mut [u64]) -> Self {
        assert!(buf.len() >= BKEY_U64S, "room of {} u64s, short of a key's header", buf.len());
        plain_bytes_mut(&mut buf[..BKEY_U64S]).copy_from_slice(plain_bytes(&c::bkey::new()));
        KeyRoom { buf }
    }

    /// The key at the front of @buf, as it is, with the rest of @buf to grow
    /// into.
    pub fn existing(buf: &'b mut [u64]) -> Self {
        assert!(buf.len() >= BKEY_U64S, "room of {} u64s, short of a key's header", buf.len());
        KeyRoom { buf }
    }

    pub fn key(&self) -> &BkeyI {
        keys(self.buf).next().expect("a key in its room")
    }

    pub fn key_mut(&mut self) -> &mut BkeyI {
        key_mut(self.buf)
    }

    pub fn k(&self) -> &c::bkey {
        &self.key().k
    }

    /// Its header - its u64s, set_val_u64s()'s.
    pub fn k_mut(&mut self) -> &mut c::bkey {
        &mut self.key_mut().k
    }

    /// Its value, as long as its u64s say.
    pub fn val_mut(&mut self) -> &mut [u64] {
        let u64s = usize::from(self.k().u64s);
        &mut self.buf[BKEY_U64S..u64s]
    }

    /// Its value made @val_u64s long; grown, the new u64s are zeroes. NoRoom
    /// past the room, or what a key's u8 of u64s holds.
    pub fn set_val_u64s(&mut self, val_u64s: usize) -> Result<(), NoRoom> {
        let need_u64s = BKEY_U64S + val_u64s;
        let have_u64s = self.buf.len().min(u8::MAX.into());
        if need_u64s > have_u64s {
            return Err(NoRoom { need_u64s, have_u64s });
        }

        let u64s = usize::from(self.k().u64s);
        if need_u64s > u64s {
            self.buf[u64s..need_u64s].fill(0);
        }
        self.k_mut().u64s = need_u64s as u8;
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
        *self.k_mut() = *k.k;
        plain_bytes_mut(self.val_mut()).copy_from_slice(val);
        Ok(())
    }
}

/// A key kept in a struct, with room for its value: C's "struct bkey_i key;
/// u64 key_pad[N];" - a field of one declares that with #[c_anon(..)]. U64S
/// is the room, the key's header included. Zeroed, as C's structs start,
/// it's a header and no value.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PaddedKey<const U64S: usize>([u64; U64S]);

unsafe impl<const U64S: usize> ::pin_init::Zeroable for PaddedKey<U64S> {}

impl<const U64S: usize> Default for PaddedKey<U64S> {
    fn default() -> Self {
        PaddedKey([0; U64S])
    }
}

/// Its header.
impl<const U64S: usize> core::ops::Deref for PaddedKey<U64S> {
    type Target = c::bkey_i;

    fn deref(&self) -> &c::bkey_i {
        let () = Self::ROOM;
        crate::util::vstructs::cast_plain(plain_bytes(&self.0)).expect("a padded key's header")
    }
}

impl<const U64S: usize> PaddedKey<U64S> {
    const ROOM: () = assert!(U64S >= BKEY_U64S, "a padded key's room, short of a key's header");

    fn check(&self) {
        let u64s = usize::from(self.k.u64s);
        assert!(u64s <= U64S, "a key of {u64s} u64s in a room of {U64S}");
    }

    pub fn key(&self) -> &BkeyI {
        self.check();
        unsafe { &*BkeyI::from_raw(self.0.as_ptr().cast()) }
    }

    pub fn key_mut(&mut self) -> &mut BkeyI {
        self.check();
        unsafe { &mut *BkeyI::from_raw_mut(self.0.as_mut_ptr().cast()) }
    }

    pub fn set_key(&mut self, k: &BkeyI) {
        let bytes = k.as_bytes();
        let room = plain_bytes_mut(&mut self.0);
        assert!(bytes.len() <= room.len(), "a key of {} bytes in a room of {}", bytes.len(), room.len());
        room[..bytes.len()].copy_from_slice(bytes);
    }

    /// All of its room, header first: for a key made in place, as by
    /// unpacking one. What reads it checks the header against the room.
    pub fn u64s_mut(&mut self) -> &mut [u64; U64S] {
        &mut self.0
    }

    /// The key, to grow in its room.
    pub fn room(&mut self) -> KeyRoom<'_> {
        KeyRoom::existing(&mut self.0)
    }
}

/// The keys in @u64s, from where a key starts.
pub(crate) fn keys(u64s: &[u64]) -> impl Iterator<Item = &BkeyI> + '_ {
    let len = u64s.len();
    VstructRefEntries::<c::bkey_i>::new(plain_bytes(u64s)).map(move |k| {
        let k = k.unwrap_or_else(|e| panic!("a key overruns its buffer, of {len} u64s: {e}"));
        // SAFETY: a keylist's keys are made in memory, by what adds them
        unsafe { assume_validated(k) }.key()
    })
}

/// The key at the front of @u64s.
pub(crate) fn key_mut(u64s: &mut [u64]) -> &mut BkeyI {
    let len = u64s.len();
    let k = VstructRefEntriesMut::<c::bkey_i>::new(plain_bytes_mut(u64s)).next()
        .expect("a key in an empty buffer")
        .unwrap_or_else(|e| panic!("a key overruns its buffer, of {len} u64s: {e}"));
    unsafe { assume_validated(k) }.into_key_mut()
}

/// Its keys in increasing order - debug builds': keylist.h's no-op otherwise.
#[cfg(CONFIG_BCACHEFS_DEBUG)]
#[cstruct_macros::c_export]
pub fn bch2_verify_keylist_sorted(l: &c::keylist) {
    let mut keys = keys(l.as_u64s()).peekable();
    while let Some(k) = keys.next() {
        if let Some(next) = keys.peek() {
            assert!(k.k().p < next.k().p, "keylist out of order: {} before {}", k.k(), next.k());
        }
    }
}
