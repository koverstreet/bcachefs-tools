// SPDX-License-Identifier: GPL-2.0

//! # The buffer
//!
//! One RcuArray: the seeds, as its header, and 2^bits slots. A key has a slot
//! in each way, which its hash gives; a slot holds an item
//! (CuckooItem) in one atomic word - a key, complemented, so the zeroes a
//! buffer is allocated as are empty and u64::MAX can't be one, or a pointer
//! to an entry, null for none. A lookup checks a slot per way: at most that
//! many cache misses.
//!
//! The hasher's built from the seeds - S: From<[u64; 2]> - and keyed by them,
//! so a key's slots are as if independent, whatever structure the keys have:
//! two from one hash - i, and i ^ delta, delta odd, so each is the other's
//! other - and for three ways a third from a second hasher, its key's first
//! word another seed.
//!
//! The ways. Two fill to about half the table, three to most of it, so the
//! table grows - a rehash, into a buffer twice the size with new seeds -
//! before an insert would take it past 45%, for two, short of where inserts
//! start to fail, or three quarters, for three. An insert that finds no room
//! anyway rehashes with the item - new seeds at that size, then the next size
//! up - and one that can't allocate fails with the table as it was. A table
//! only shrinks by clear().
//!
//! The seeds and bits are mirrored outside the buffer, so a lookup hashes
//! while it follows the pointer, not after. A rehash can race that, pairing
//! one buffer's seeds with another's slots: a hit is still a hit - the key's
//! in the buffer - and only a miss compares mirror and buffer, looking again
//! with the buffer's own seeds on the rare mismatch.
//!
//! # Readers and writers
//!
//! Writers take the table's lock, a ByteLock - a mutex, zeroed unlocked, so a
//! table is still valid zeroed - so they're one at a time, each seeing slots
//! only it changes, and a rehash allocates, and sleeps, under it. (One lock
//! for now; lockless writers, by cmpxchg, are for later.) A reader takes
//! nothing, and sees a key a write isn't touching, always:
//!
//! - A slot changes by one atomic store: an item into an empty slot, or empty
//!   into one with an item. A reader loads it once and compares the key - the
//!   item's, or its entry's. An entry's key doesn't change while it's in a
//!   table, and it stays valid until a grace period after it's out
//!   (Entry::new()), so a hit returns its key's entry whatever the slot held
//!   before or since.
//! - An insert that has to evict finds its chain first - each occupant to
//!   another of its slots - to an empty one, changing nothing, then moves it
//!   from the empty end: each item is copied into a slot that's empty before
//!   its own is emptied, so every item is in one of its slots at every
//!   moment. A lookup can still miss one that moved during it - reading its
//!   new slot before the copy, its old one after - so the writer counts each
//!   move between the copy and the emptying, and a lookup that misses looks
//!   again between two loads of the count, retrying if it changed. A hit
//!   needs no count, and doesn't read it. So nothing waits on the writer -
//!   one preempted mid-chain leaves every item findable - and a hit reads
//!   nothing the writer writes but the slots.
//!
//! A writer's view of the buffer, locked(), is a borrow the types don't end:
//! rehash() and clear() free the buffer after a grace period the writer isn't
//! in, and take &self like everything else. So this file keeps to it:
//! rehash() reads the old buffer only in its fill, before the replace, and
//! put() takes no view across one.

use crate::data_structures::array_vec::ArrayVec;
use core::borrow::Borrow;
use core::hash::{BuildHasher, Hash};
use core::marker::PhantomData;
use core::sync::atomic::{fence, AtomicU32, AtomicU64};
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};

use super::{Cuckoo, CuckooItem, Faults, Held, Mode};
use crate::util::alloc::AllocError;
use crate::util::kernel::random_u64;
use crate::util::rcu::{RcuElement, ReadGuard};

/// The longest chain of moves an insert follows before it rehashes.
const MAX_KICKS: usize = 32;

/// An insert's chain of slots: u32s, as an RcuArray is at most INT_MAX bytes.
type Path = ArrayVec<u32, { MAX_KICKS + 1 }>;

/// The smallest table.
const MIN_BITS: u32 = 2;

/// Count a rare path's running, in Faults::seen.
macro_rules! saw {
    ($faults:expr, $what:ident) => {
        #[cfg(CONFIG_BCACHEFS_TESTS)]
        $faults.seen.$what.fetch_add(1, Relaxed);
    };
}

/// A buffer's hashers' keys: its first, and a third way's first word.
pub(super) type Seeds = [u64; 3];

// ── The hash ─────────────────────────────────────────────────────────────

/// @key's slot in each of @W ways, in a table of 2^@bits hashed by @S keyed
/// with @seeds. For zero @bits - a Mirror not yet written - any index, in the
/// table or past it.
fn slots<const W: usize, S: BuildHasher + From<[u64; 2]>, K: ?Sized + Hash>(seeds: Seeds, bits: u32, key: &K) -> [usize; W] {
    let hash = |k0| S::from([k0, seeds[1]]).hash_one(key);
    let mask = (1usize << bits) - 1;
    let h = hash(seeds[0]);
    // the top bits: a multiplicative hash's best
    let i = h.wrapping_shr(64 - bits) as usize & mask;
    core::array::from_fn(|w| match w {
        0 => i,
        1 => i ^ (((h >> 32) as usize | 1) & mask),
        _ => hash(seeds[2]).wrapping_shr(64 - bits) as usize & mask,
    })
}

#[repr(transparent)]
pub(super) struct Slot<I: CuckooItem>(I::Atomic);

// SAFETY: CuckooItem's: owning nothing, and all zeroes empty
unsafe impl<I: CuckooItem> RcuElement for Slot<I> {}

impl<I: CuckooItem> Slot<I> {
    /// A reader's view.
    fn load(&self) -> Option<I> {
        I::load(&self.0, Acquire)
    }

    /// The writer's view.
    pub(super) fn load_locked(&self) -> Option<I> {
        I::load(&self.0, Relaxed)
    }

    fn set(&self, i: I) {
        I::store(&self.0, Some(i), Release)
    }

    pub(super) fn clear(&self) {
        I::store(&self.0, None, Release)
    }
}

/// The published buffer's seeds and bits - or another buffer's, if a rehash
/// raced, naming the wrong slots or ones past the end.
pub(super) struct Mirror {
    seeds: [AtomicU64; 3],
    bits:  AtomicU32,
}

impl Mirror {
    fn load(&self) -> (Seeds, u32) {
        (self.seeds.each_ref().map(|s| s.load(Relaxed)), self.bits.load(Relaxed))
    }

    fn store(&self, seeds: &Seeds, bits: u32) {
        self.seeds.iter().zip(seeds).for_each(|(m, &s)| m.store(s, Relaxed));
        self.bits.store(bits, Relaxed);
    }
}

/// Whether 2^@bits slots of @W ways take @nr items: see the top of the file.
fn fits<const W: usize>(bits: u32, nr: usize, faults: &Faults) -> bool {
    let slots = 1usize << bits;
    if faults.dense() {
        nr <= slots
    } else if W == 2 {
        nr * 20 <= slots * 9
    } else {
        nr * 4 <= slots * 3
    }
}

/// A buffer's seeds and slots, as a reader has them in its read-side section,
/// or the writer: so its items are live.
pub(super) struct Table<'a, I: CuckooItem, S, const W: usize> {
    seeds:  Seeds,
    pub(super) slots: &'a [Slot<I>],
    faults: &'a Faults,
    hash:   PhantomData<fn() -> S>,
}

impl<'a, I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize> Table<'a, I, S, W> {
    fn new(seeds: Seeds, slots: &'a [Slot<I>], faults: &'a Faults) -> Self {
        Self { seeds, slots, faults, hash: PhantomData }
    }

    fn bits(&self) -> u32 {
        self.slots.len().trailing_zeros()
    }

    fn index<K: ?Sized + Hash>(&self, key: &K) -> [usize; W] {
        slots::<W, S, K>(self.seeds, self.bits(), key)
    }

    pub(super) fn key<'i>(&self, i: &'i I) -> &'i I::Target {
        // SAFETY: a Table's items are live: see the struct
        unsafe { i.target() }
    }

    /// @key's item, if slot @i - which may be past this buffer - holds it.
    fn get<Q: ?Sized + Eq>(&self, i: usize, key: &Q) -> Option<I>
    where I::Target: Borrow<Q>
    {
        self.slots.get(i)?.load().filter(|it| self.key(it).borrow() == key)
    }

    /// @key's item, from the slots this buffer's seeds name: for a lookup
    /// whose Mirror was another buffer's.
    #[cold]
    fn get_raced<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<I>
    where I::Target: Borrow<Q>
    {
        saw!(self.faults, raced);
        self.index(key).into_iter().find_map(|i| self.get(i, key))
    }

    /// Writer: the slot holding @key.
    pub(super) fn find_locked<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<&'a Slot<I>>
    where I::Target: Borrow<Q>
    {
        self.index(key).into_iter()
            .map(|i| &self.slots[i])
            .find(|s| s.load_locked().is_some_and(|i| self.key(&i).borrow() == key))
    }

    /// Writer: what the table holds.
    fn iter_locked(&self) -> impl Iterator<Item = I> + use<'a, I, S, W> {
        self.slots.iter().filter_map(Slot::load_locked)
    }

    /// Where @key, which isn't in the table, can go, into @path: one of its
    /// slots, empty, or a chain from one - walk(). False if there's none.
    fn find_path(&self, key: &I::Target, path: &mut Path) -> bool {
        let own = self.index(key);
        if let Some(&i) = own.iter().find(|&&i| self.slots[i].load_locked().is_none()) {
            path.push(i as u32);
            return true;
        }
        own.into_iter().any(|start| {
            path.clear();
            self.walk(start, path)
        })
    }

    /// A chain of slots from @start to an empty one, into @path: each the one
    /// before's occupant's other slot - an empty one if it has one, else the
    /// last not on the chain - and none twice, so moving them from the empty
    /// end empties each in turn for the one before. False if it comes to
    /// nothing within MAX_KICKS moves.
    fn walk(&self, start: usize, path: &mut Path) -> bool {
        let mut s = start;

        loop {
            if path.try_push(s as u32).is_err() {
                return false;
            }
            let Some(item) = self.slots[s].load_locked() else { return true };

            let others = self.index(self.key(&item)).into_iter().filter(|&o| !path.contains(&(o as u32)));
            let empty = others.clone().find(|&o| self.slots[o].load_locked().is_none());
            let Some(n) = empty.or_else(|| others.last()) else { return false };
            s = n;
        }
    }

    /// Insert @item, whose key isn't in the table, along find_path()'s chain,
    /// counting each move in @moves: false, and nothing moved, if there's no
    /// chain.
    fn insert(&self, item: I, moves: &AtomicU64) -> bool {
        let mut path = Path::new();
        if !self.find_path(self.key(&item), &mut path) {
            return false;
        }

        for w in path.windows(2).rev() {
            let (from, to) = (&self.slots[w[0] as usize], &self.slots[w[1] as usize]);
            to.set(from.load_locked().expect("on the path: occupied"));
            self.faults.move_window();
            moves.fetch_add(1, Release);
            self.faults.move_window();
            from.clear();
        }
        self.slots[path[0] as usize].set(item);
        true
    }
}

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize, M: Mode> Cuckoo<I, S, W, M> {
    const WAYS: () = assert!(W == 2 || W == 3, "cuckoo: two ways or three");

    /// @key's item, under @g's section.
    pub(super) fn lookup<Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &ReadGuard) -> Option<I>
    where I::Target: Borrow<Q>
    {
        let mirror = self.mirror.load();
        let () = Self::WAYS;
        let slots = slots::<W, S, Q>(mirror.0, mirror.1, key);

        if let Some(i) = self.probe(key, g, mirror, slots) {
            return Some(i);
        }
        loop {
            let moves = self.moves.load(Acquire);
            if let Some(i) = self.probe(key, g, mirror, slots) {
                return Some(i);
            }
            fence(Acquire);
            if self.moves.load(Relaxed) == moves {
                return None;
            }
            saw!(self.faults, retry);
        }
    }

    /// @key's item, from @slots - its, under @mirror, which may be another
    /// buffer's - or from the buffer's own if the mirror was stale.
    fn probe<Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &ReadGuard, mirror: (Seeds, u32), slots: [usize; W])
        -> Option<I>
    where I::Target: Borrow<Q>
    {
        let (seeds, buf) = self.table.read_head(g)?;
        let t = Table::<I, S, W>::new(*seeds, buf, &self.faults);

        let hit = slots.into_iter().find_map(|i| {
            self.faults.read_window();
            t.get(i, key)
        });
        hit.or_else(|| (mirror != (*seeds, t.bits())).then(|| t.get_raced(key)).flatten())
    }
}

// ── Writers ──────────────────────────────────────────────────────────────

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize, M: Mode> Cuckoo<I, S, W, M> {
    /// The buffer, as the holder of the writers' lock, @g, has it: not to be
    /// kept past a rehash() or clear() - see the top of the file.
    pub(super) fn locked<'a>(&'a self, g: &Held<'_>) -> Option<Table<'a, I, S, W>> {
        debug_assert!(core::ptr::eq(g.lock(), &self.lock));
        // SAFETY: we hold the writers' lock, and no replace() comes while the
        // view is in use
        unsafe { self.table.get_head_locked() }
            .map(|(seeds, slots)| Table::new(*seeds, slots, &self.faults))
    }

    /// insert(), or if not @replace insert_new(): what had @item's key.
    pub(super) fn put(&self, g: &Held<'_>, item: I, replace: bool) -> Result<Option<I>, AllocError> {
        let () = Self::WAYS;
        assert!(item.storable(), "cuckoo: an item that can't be in a table");

        let t = self.locked(g);
        if let Some(s) = t.as_ref().and_then(|t| t.find_locked(t.key(&item))) {
            let old = s.load_locked().expect("found: occupied");
            if replace {
                s.set(item);
            }
            return Ok(Some(old));
        }

        let nr = self.nr.load(Relaxed) + 1;
        let room = t.filter(|t| fits::<W>(t.bits(), nr, &self.faults));
        if !room.is_some_and(|t| t.insert(item, &self.moves)) {
            return self.rehash(g, item).map(|()| None);
        }
        self.nr.store(nr, Relaxed);
        Ok(None)
    }

    /// A new buffer with every item, and @item: see the top of the file. On
    /// an error the table is unchanged.
    fn rehash(&self, g: &Held<'_>, item: I) -> Result<(), AllocError> {
        let old = self.locked(g);
        let nr = self.nr.load(Relaxed) + 1;

        let faults = &self.faults;
        let mut bits = old.as_ref().map_or(MIN_BITS, Table::bits);
        while !fits::<W>(bits, nr, faults) {
            bits += 1;
        }

        // The new buffer's moves are before anyone can see it: not counted,
        // and no windows
        let (moves, unpublished) = (AtomicU64::new(0), Faults::default());

        loop {
            if faults.alloc_fail() {
                return Err(AllocError);
            }
            for _ in 0..3 {
                let seeds: Seeds = core::array::from_fn(|_| random_u64());
                // SAFETY: we hold the writers' lock; @old's slots don't
                // change under us, and nothing reads them past the replace()
                let fit = unsafe {
                    self.table.rebuild_head(1 << bits, seeds, |slots| {
                        let t = Table::<I, S, W>::new(seeds, slots, &unpublished);
                        old.iter().flat_map(Table::iter_locked).chain([item])
                            .all(|i| t.insert(i, &moves))
                            .then_some(()).ok_or(())
                    })
                }?;
                if fit.is_ok() {
                    saw!(faults, rehash);
                    self.mirror.store(&seeds, bits);
                    self.nr.store(nr, Relaxed);
                    return Ok(());
                }
            }
            saw!(faults, grown);
            bits += 1;
        }
    }
}
