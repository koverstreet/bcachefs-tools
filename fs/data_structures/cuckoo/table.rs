// SPDX-License-Identifier: GPL-2.0

//! # Terms
//!
//! - **Buffer**: one RcuArray - the seeds as its header, then 2^bits slots.
//!   Replaced whole by a rehash, and freed a grace period after.
//! - **Slot**: one atomic word, empty or holding an item (CuckooItem): a u64
//!   key, complemented so that zero is empty, or a pointer to an entry, null
//!   for empty.
//! - **A key's slots**: one per way. The hash is keyed by the buffer's seeds,
//!   so a key's slots are as if independent, however the keys are
//!   structured. Slot 0 is the hash's top bits, a multiplicative hash's good
//!   ones. Slot 1 is slot 0 xor an odd delta from the same hash, so each of
//!   the two is the other's other. Slot 2, for three ways, comes from a second
//!   hash keyed by the third seed. An item is always in one of its key's
//!   slots.
//! - **Load limit**: a buffer holds items in at most 45% of its slots with two
//!   ways, 75% with three, short of where inserts start to fail.
//! - **Chain**: for an insert whose item's slots are all full, a path of slots
//!   from one of them to an empty one. Each next slot is another of the
//!   previous slot's occupant's slots: an empty one if it has one, else the
//!   last not already on the chain. At most MAX_KICKS moves.
//! - **Move**: an occupant copied to the next slot on its chain, the move
//!   counted (Cuckoo::moves), then its old slot cleared. A chain is moved from
//!   its empty end, so every item is in one of its slots at every moment.
//! - **Mirror**: the published buffer's seeds and bits, kept outside it so
//!   that a lookup hashes while it loads the buffer pointer. A rehash racing
//!   a lookup can leave it naming another buffer's slots, wrong ones or ones
//!   past the end: a lookup's slot reads are bounds-checked.
//! - **Table**: a view of one buffer, held in a read-side section or by the
//!   writer, so that the items it reads are live.
//! - **_locked**: the writer's, under the writers' lock - Relaxed, as only the
//!   writer changes slots.
//!
//! # Lookups
//!
//! A slot changes by one atomic store. A lookup loads each of the key's slots
//! once, by the mirror, and compares its key - the item's, or its entry's,
//! which doesn't change while the entry is in a table and stays valid a grace
//! period after it's out (Entry::new()). So a hit returns its key's item,
//! whatever the slot held before or since; besides slots, it reads only what
//! a rehash writes.
//!
//! A lookup can miss an item that moved during it, reading the new slot
//! before the copy and the old one after the clear. So a miss probes again
//! between two loads of the move count, and retries if it changed - and if
//! the mirror wasn't the buffer's, looks again by the buffer's own seeds.
//! Nothing waits on the writer: one preempted mid-chain leaves every item
//! findable.
//!
//! # Writers
//!
//! Writers are one at a time, under the writers' lock. An insert that would
//! take the table over the load limit, or finds no chain, rehashes with the
//! item: fills a new buffer with every item, new seeds SEED_TRIES times at a
//! size, then the next size up, and publishes it, the mirror after it. The
//! fill is unpublished: its moves aren't counted, and get no fault windows.
//! One that can't allocate leaves the table as it was. A table only shrinks
//! by clear().
//!
//! A writer's view, locked(), is a borrow the types don't end: rehash() and
//! empty() free the buffer a grace period on, and the writer isn't in one.
//! So rehash() reads the old buffer only in its fill, before the replace, and
//! put() keeps no view across one.

use crate::data_structures::array_vec::ArrayVec;
use core::borrow::Borrow;
use core::hash::Hash;
use core::marker::PhantomData;
use core::sync::atomic::{fence, AtomicU32, AtomicU64};
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};

use super::{Cuckoo, CuckooHasher, CuckooItem, Faults, Held, Mode};
use crate::util::alloc::AllocError;
use crate::util::kernel::random_u64;
use crate::util::rcu::{RcuElement, ReadGuard};

const MAX_KICKS: usize = 32;

/// u32s, as an RcuArray is at most INT_MAX bytes.
type Path = ArrayVec<u32, { MAX_KICKS + 1 }>;

const MIN_BITS: u32 = 2;

const SEED_TRIES: usize = 3;

macro_rules! saw {
    ($faults:expr, $what:ident) => {
        #[cfg(CONFIG_BCACHEFS_TESTS)]
        $faults.seen.$what.fetch_add(1, Relaxed);
    };
}

pub(super) type Seeds = [u64; 3];

fn key_slots<const W: usize, S: CuckooHasher, K: ?Sized + Hash>((seeds, bits): (Seeds, u32), key: &K) -> [usize; W] {
    const { assert!(W == 2 || W == 3, "cuckoo: two ways or three") };
    let hash = |k0| S::from([k0, seeds[1]]).hash_one(key);
    let mask = (1usize << bits) - 1;
    let h = hash(seeds[0]);
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
    fn load(&self) -> Option<I> {
        I::load(&self.0, Acquire)
    }

    fn load_locked(&self) -> Option<I> {
        I::load(&self.0, Relaxed)
    }

    fn set(&self, i: I) {
        I::store(&self.0, Some(i), Release)
    }

    fn clear(&self) {
        I::store(&self.0, None, Release)
    }
}

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

struct Table<'a, I: CuckooItem, S, const W: usize> {
    seeds:  Seeds,
    slots:  &'a [Slot<I>],
    faults: &'a Faults,
    hash:   PhantomData<fn() -> S>,
}

impl<'a, I: CuckooItem, S: CuckooHasher, const W: usize> Table<'a, I, S, W> {
    fn new(seeds: Seeds, slots: &'a [Slot<I>], faults: &'a Faults) -> Self {
        Self { seeds, slots, faults, hash: PhantomData }
    }

    fn bits(&self) -> u32 {
        self.slots.len().trailing_zeros()
    }

    fn index<K: ?Sized + Hash>(&self, key: &K) -> [usize; W] {
        key_slots::<W, S, K>((self.seeds, self.bits()), key)
    }

    fn key<'i>(&self, i: &'i I) -> &'i I::Target {
        // SAFETY: a Table's items are live
        unsafe { i.target() }
    }

    fn get<Q: ?Sized + Eq>(&self, i: usize, key: &Q) -> Option<I>
    where I::Target: Borrow<Q>
    {
        self.slots.get(i)?.load().filter(|it| self.key(it).borrow() == key)
    }

    #[cold]
    fn get_raced<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<I>
    where I::Target: Borrow<Q>
    {
        saw!(self.faults, raced);
        self.index(key).into_iter().find_map(|i| self.get(i, key))
    }

    fn find_locked<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<(&'a Slot<I>, I)>
    where I::Target: Borrow<Q>
    {
        self.index(key).into_iter().find_map(|i| {
            let s = &self.slots[i];
            s.load_locked().filter(|it| self.key(it).borrow() == key).map(|it| (s, it))
        })
    }

    fn iter_locked(&self) -> impl Iterator<Item = I> + use<'a, I, S, W> {
        self.slots.iter().filter_map(Slot::load_locked)
    }

    fn find_path(&self, key: &I::Target) -> Option<Path> {
        let own = self.index(key);
        if let Some(i) = own.into_iter().find(|&i| self.slots[i].load_locked().is_none()) {
            return self.walk(i);
        }
        own.into_iter().find_map(|start| self.walk(start))
    }

    fn walk(&self, mut s: usize) -> Option<Path> {
        let mut path = Path::new();

        loop {
            path.try_push(s as u32).ok()?;
            let Some(item) = self.slots[s].load_locked() else { return Some(path) };

            let others = self.index(self.key(&item)).into_iter().filter(|&o| !path.contains(&(o as u32)));
            let empty = others.clone().find(|&o| self.slots[o].load_locked().is_none());
            s = empty.or_else(|| others.last())?;
        }
    }

    /// @item's key isn't in the table. False, with nothing moved, if there's
    /// no chain.
    fn insert(&self, item: I, moves: &AtomicU64) -> bool {
        let Some(path) = self.find_path(self.key(&item)) else { return false };

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

impl<I: CuckooItem, S: CuckooHasher, const W: usize, M: Mode> Cuckoo<I, S, W, M> {
    pub(super) fn lookup<Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &ReadGuard) -> Option<I>
    where I::Target: Borrow<Q>
    {
        let mirror = self.mirror.load();
        let slots = key_slots::<W, S, Q>(mirror, key);

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
        if hit.is_none() && mirror != (*seeds, t.bits()) {
            return t.get_raced(key);
        }
        hit
    }

    /// Not to be kept across a rehash() or empty().
    fn locked<'a>(&'a self, g: &Held<'_>) -> Option<Table<'a, I, S, W>> {
        debug_assert!(core::ptr::eq(g.lock(), &self.lock));
        // SAFETY: we hold the writers' lock, and no replace() comes while the
        // view is in use
        unsafe { self.table.get_head_locked() }
            .map(|(seeds, slots)| Table::new(*seeds, slots, &self.faults))
    }

    pub(super) fn put(&self, g: &Held<'_>, item: I, replace: bool) -> Result<Option<I>, AllocError> {
        assert!(item.storable(), "cuckoo: an item that can't be in a table");

        let t = self.locked(g);
        if let Some((s, old)) = t.as_ref().and_then(|t| t.find_locked(t.key(&item))) {
            if replace {
                s.set(item);
            }
            return Ok(Some(old));
        }

        let nr = self.nr.load(Relaxed) + 1;
        let room = t.filter(|t| fits::<W>(t.bits(), nr, &self.faults));
        if !room.is_some_and(|t| t.insert(item, &self.moves)) {
            return self.rehash(g, item, nr).map(|()| None);
        }
        self.nr.store(nr, Relaxed);
        Ok(None)
    }

    pub(super) fn take_if<Q: ?Sized + Hash + Eq>(&self, g: &Held<'_>, key: &Q, pred: impl FnOnce(&I) -> bool) -> Option<I>
    where I::Target: Borrow<Q>
    {
        let (s, i) = self.locked(g)?.find_locked(key).filter(|(_, i)| pred(i))?;
        s.clear();
        self.nr.fetch_sub(1, Relaxed);
        Some(i)
    }

    pub(super) fn empty(&self, g: &Held<'_>) {
        debug_assert!(core::ptr::eq(g.lock(), &self.lock));
        // SAFETY: we hold the writers' lock
        unsafe { self.table.clear() }
        self.nr.store(0, Relaxed);
    }

    pub(super) fn check_invariant(&self, g: &Held<'_>) -> Result<usize, &'static str> {
        let Some(t) = self.locked(g) else { return Ok(0) };
        let mut nr = 0;

        for s in t.slots {
            let Some(i) = s.load_locked() else { continue };
            match t.find_locked(t.key(&i)) {
                None => return Err("cuckoo: an item in a slot it doesn't hash to"),
                Some((f, _)) if !core::ptr::eq(f, s) => return Err("cuckoo: an item in two slots"),
                Some(_) => nr += 1,
            }
        }
        if nr != self.nr.load(Relaxed) {
            return Err("cuckoo: an item count that isn't the items");
        }
        Ok(nr)
    }

    fn rehash(&self, g: &Held<'_>, item: I, nr: usize) -> Result<(), AllocError> {
        let old = self.locked(g);

        let faults = &self.faults;
        let mut bits = old.as_ref().map_or(MIN_BITS, Table::bits);
        while !fits::<W>(bits, nr, faults) {
            bits += 1;
        }

        let (moves, unpublished) = (AtomicU64::new(0), Faults::default());

        loop {
            if faults.alloc_fail() {
                return Err(AllocError);
            }
            for _ in 0..SEED_TRIES {
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
