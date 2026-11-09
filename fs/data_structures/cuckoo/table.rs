// SPDX-License-Identifier: GPL-2.0

//! # Terms
//!
//! - **Buffer**: one RcuArray - a header, then 2^bits slots. The header is the
//!   buffer's seeds, and the next newer table's buffer.
//! - **Tables**: the oldest buffer, published, and the newer ones linked from
//!   it; more than one while a rehash drains the older into the newest. A
//!   drained one is unlinked, and freed a grace period on.
//! - **Slot**: one atomic word, read as Contents: empty; claimed - empty, and a
//!   writer's to store into; retired - drained, and nothing goes in it again;
//!   an item (CuckooItem); or an item marked unstable, a writer's. The mark is
//!   the word's low bit.
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
//! - **Claimed chain**: one a writer holds - its empty end claimed, then each
//!   item on it marked, from the end back, each by a cmpxchg against what the
//!   chain found there - so no other writer changes it. Its items are moved
//!   from the empty end by plain stores, each move counted (Cuckoo::moves)
//!   between filling its new slot and overwriting its old.
//! - **Settle**: an insert's last step, its item stored marked in the slot its
//!   claimed chain freed, and a SeqCst fence past: withdrawn if a newer table
//!   has been linked, or its key's in an older table or another of its slots
//!   in this one - each table's found by a scan no move ran during, as a
//!   lookup's probe is; else unmarked. A marked key in an older table is
//!   waited out, and in this one by the key's slot 0, which wins.
//! - **Drain**: each of the oldest table's slots retired: an empty one by a
//!   cmpxchg; one with an item by placing it, marked, in the newest by a
//!   claimed chain, taking it from the old slot by a cmpxchg to retired, and
//!   unmarking it - or emptying the placement, if the old slot had changed.
//! - **Mirror**: the newest table's seeds and bits, kept outside it so that a
//!   lookup hashes while it loads the buffer pointer. A rehash racing a
//!   lookup can leave it naming another buffer's slots, wrong ones or ones
//!   past the end: a lookup's slot reads are bounds-checked.
//! - **Table**: a view of one buffer, held in a read-side section, so that the
//!   items it reads are live.
//!
//! # Lookups
//!
//! A lookup loads each of the key's slots once and compares its key - the
//! item's, or its entry's, which doesn't change while the entry is in a table
//! and stays valid a grace period after it's out (Entry::new()). An unmarked
//! item is one that's in: a hit returns it.
//!
//! The first probe is the oldest table's, by the mirror: with one table, the
//! newest, that's a hit, and it reads nothing but the slots - not even the
//! table's header. A wrong slot holding the key is a hit too.
//!
//! On a miss a lookup takes each table, oldest to newest, by its own seeds -
//! the mirror's slots, if they're its - until a probe that meets nothing
//! unstable, a claim or a mark being waited out, and that no move ran during:
//! one that missed an item moved under it, its new slot read before the fill
//! and its old one after the overwrite, sees the move count change. A drain
//! places an item in the newer table before it retires the old slot, so a
//! lookup that found the old slot retired finds the item in a later table:
//! drains count nothing.
//!
//! A lookup waits on a writer only while the writer holds a claim or a mark,
//! with preemption off. Not IRQ safe: an interrupt on that writer's CPU that
//! looks the table up and meets one spins on a writer that can't run until
//! it returns. Making it safe would be a type parameter, as Mode is, that
//! turns interrupts off, not just preemption, wherever a writer holds one.
//!
//! # Writers
//!
//! Inserts and removes take no lock; the table's lock is rehashes', and
//! clear()'s. An insert looks for its key in every table, waiting out
//! anything unstable, then claims a chain in the newest table, moves along
//! it, and settles. One that would take the newest over the load limit, or
//! finds no chain, rehashes. A remove or a replace is a cmpxchg of an
//! unmarked item.
//!
//! Two inserts of one key, or an insert and a rehash, each store, fence and
//! load, so one always sees the other: two in one table each other's mark;
//! an insert and a rehash the newer table's link, or the insert's mark.
//! Across a rehash, an insert into the newer table seeing one committed in
//! the older relies on full fences being cumulative, as they are on every
//! architecture Linux runs on.
//!
//! A rehash links a newer table, the smallest that fits, and drains the
//! older: a newest that can't place an item gets a bigger one linked after
//! it, and an allocation failure partway leaves the rest to the next rehash.
//! clear() retires every slot, dropping the items, and unlinks every table,
//! so an insert still on one meets a retired slot or no table and goes on. A
//! table only shrinks by clear().
//!
//! A wait is from a newer table on an older, from a key's slot 0 on its
//! others, or by a writer holding nothing: none is part of a cycle.
//!
//! A view under the rehash lock, locked(), is a borrow the types don't end:
//! drain() and empty() free buffers a grace period on. So drain() reads a
//! table only until it unlinks it.

use crate::data_structures::array_vec::ArrayVec;
use core::borrow::Borrow;
use core::hash::Hash;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicPtr, AtomicU32, AtomicU64};
use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};

use super::{Contents, Cuckoo, CuckooHasher, CuckooItem, Faults, Held, Mode};
use crate::util::alloc::AllocError;
use crate::util::kernel::random_u64;
use crate::util::percpu::CpuGuard;
use crate::util::rcu::{kvfree_rcu, read_lock, RcuArray, RcuArrayHead, RcuElement, ReadGuard};

const MAX_KICKS: usize = 32;

const MIN_BITS: u32 = 2;

macro_rules! saw {
    ($faults:expr, $what:ident) => {
        #[cfg(CONFIG_BCACHEFS_TESTS)]
        $faults.seen.$what.fetch_add(1, Relaxed);
    };
}

type Seeds = [u64; 3];

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

fn key_of<I: CuckooItem>(item: &I) -> &I::Target {
    // SAFETY: an item's one a writer is putting in, or one read from a slot
    // in a read-side section: live
    unsafe { item.target() }
}

#[repr(transparent)]
pub(super) struct Slot<I: CuckooItem>(I::Atomic);

// SAFETY: CuckooItem's: owning nothing, and all zeroes empty
unsafe impl<I: CuckooItem> RcuElement for Slot<I> {}

impl<I: CuckooItem> Slot<I> {
    fn load(&self) -> Contents<I> {
        I::load(&self.0, Acquire)
    }

    fn store(&self, v: Contents<I>) {
        I::store(&self.0, v, Release)
    }

    fn cas(&self, current: Contents<I>, new: Contents<I>) -> bool {
        I::compare_exchange(&self.0, current, new, AcqRel, Acquire).is_ok()
    }

    fn is_empty(&self) -> bool {
        matches!(self.load(), Contents::Empty)
    }
}

/// Why an item didn't go in a table.
enum NotIn {
    NoChain,
    /// Another writer's claim or mark on the chain, or a change since it was
    /// found: let go of, waited out, and tried again.
    Busy,
    /// A drain has been through it: the item goes in a newer table.
    Retired,
}

/// What a probe of a key's slots found.
enum Probe<'a, I: CuckooItem> {
    Hit(&'a Slot<I>, I),
    /// A claim or a mark among them: what it settles to may be the key.
    Unstable,
    Miss,
}

/// From an item's slot to an empty one: the occupied slots, with the item seen
/// in each, and the empty one at the end. u32s, as an RcuArray is at most
/// INT_MAX bytes.
struct Chain<I> {
    steps: ArrayVec<(u32, I), MAX_KICKS>,
    end:   u32,
}

pub(super) struct Head {
    seeds: Seeds,
    next:  AtomicPtr<RcuArrayHead<Head>>,
}

// SAFETY: integers, and a pointer to what the Cuckoo frees, not this: all
// zeroes is no next
unsafe impl RcuElement for Head {}

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
    head:   &'a Head,
    slots:  &'a [Slot<I>],
    faults: &'a Faults,
    hash:   PhantomData<fn() -> S>,
}

impl<I: CuckooItem, S, const W: usize> Clone for Table<'_, I, S, W> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<I: CuckooItem, S, const W: usize> Copy for Table<'_, I, S, W> {}

impl<'a, I: CuckooItem, S: CuckooHasher, const W: usize> Table<'a, I, S, W> {
    fn new(head: &'a Head, slots: &'a [Slot<I>], faults: &'a Faults) -> Self {
        Self { head, slots, faults, hash: PhantomData }
    }

    fn linked(&self, p: *mut RcuArrayHead<Head>) -> Option<Self> {
        // SAFETY: a table's newer ones outlive it - for a reader, its section;
        // under the rehash lock, until it unlinks them, oldest first
        NonNull::new(p).map(|p| unsafe { RcuArray::<Slot<I>, Head>::parts(p) })
            .map(|(head, slots)| Self::new(head, slots, self.faults))
    }

    fn next(&self) -> Option<Self> {
        self.linked(self.head.next.load(Acquire))
    }

    fn is_newest(&self) -> bool {
        self.head.next.load(Acquire).is_null()
    }

    fn bits(&self) -> u32 {
        self.slots.len().trailing_zeros()
    }

    fn index<K: ?Sized + Hash>(&self, key: &K) -> [usize; W] {
        key_slots::<W, S, K>((self.head.seeds, self.bits()), key)
    }

    /// The mirror's slots, if it's this table's.
    fn slots_for<K: ?Sized + Hash>(&self, key: &K, mirror: (Seeds, u32), mirrored: [usize; W]) -> [usize; W] {
        if mirror == (self.head.seeds, self.bits()) {
            return mirrored;
        }
        saw!(self.faults, raced);
        self.index(key)
    }

    fn probe<Q: ?Sized + Eq>(&self, key: &Q, slots: [usize; W]) -> Probe<'a, I>
    where I::Target: Borrow<Q>
    {
        let mut unstable = false;
        for i in slots {
            self.faults.read_window();
            let Some(s) = self.slots.get(i) else { continue };
            match s.load() {
                Contents::Item(it) if key_of(&it).borrow() == key => return Probe::Hit(s, it),
                Contents::Claimed | Contents::Unstable(_) => unstable = true,
                _ => {}
            }
        }
        if unstable { Probe::Unstable } else { Probe::Miss }
    }

    fn find_item<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<&'a Slot<I>>
    where I::Target: Borrow<Q>
    {
        self.index(key).into_iter().map(|i| &self.slots[i])
            .find(|s| s.load().item().is_some_and(|it| key_of(&it).borrow() == key))
    }

    fn find_path(&self, key: &I::Target) -> Result<Chain<I>, NotIn> {
        let own = self.index(key);
        if let Some(i) = own.into_iter().find(|&i| self.slots[i].is_empty()) {
            return Ok(Chain { steps: ArrayVec::new(), end: i as u32 });
        }
        own.into_iter().map(|start| self.walk(start))
            .find(|r| !matches!(r, Err(NotIn::NoChain)))
            .unwrap_or(Err(NotIn::NoChain))
    }

    fn walk(&self, mut s: usize) -> Result<Chain<I>, NotIn> {
        let mut steps: ArrayVec<(u32, I), MAX_KICKS> = ArrayVec::new();

        loop {
            let item = match self.slots[s].load() {
                Contents::Empty => return Ok(Chain { steps, end: s as u32 }),
                Contents::Retired => return Err(NotIn::Retired),
                Contents::Claimed | Contents::Unstable(_) => return Err(NotIn::Busy),
                Contents::Item(i) => i,
            };
            steps.try_push((s as u32, item)).map_err(|_| NotIn::NoChain)?;

            let on_chain = |o: usize| steps.iter().any(|&(c, _)| c as usize == o);
            let others = self.index(key_of(&item)).into_iter().filter(|&o| !on_chain(o));
            let empty = others.clone().find(|&o| self.slots[o].is_empty());
            s = empty.or_else(|| others.last()).ok_or(NotIn::NoChain)?;
        }
    }

    /// @chain's first slot, freed to store into: the empty end claimed, then
    /// each item marked, from the end back, and the items moved along - each
    /// store to a slot @chain claimed, so plain. The caller's preemption is
    /// off from here to its store. Busy, with every claim let go, if anything
    /// on @chain changed since it was found.
    fn claim_room(&self, chain: &Chain<I>, moves: &AtomicU64) -> Result<&'a Slot<I>, NotIn> {
        let end = &self.slots[chain.end as usize];
        if !end.cas(Contents::Empty, Contents::Claimed) {
            saw!(self.faults, busy);
            return Err(NotIn::Busy);
        }
        for (n, &(c, x)) in chain.steps.iter().enumerate().rev() {
            self.faults.move_window();
            if !self.slots[c as usize].cas(Contents::Item(x), Contents::Unstable(x)) {
                for &(c, x) in &chain.steps[n + 1..] {
                    self.slots[c as usize].store(Contents::Item(x));
                }
                end.store(Contents::Empty);
                saw!(self.faults, busy);
                return Err(NotIn::Busy);
            }
        }

        let mut to = end;
        for &(c, x) in chain.steps.iter().rev() {
            to.store(Contents::Item(x));
            self.faults.move_window();
            moves.fetch_add(1, Release);
            self.faults.move_window();
            to = &self.slots[c as usize];
        }
        Ok(to)
    }

    /// @key in one of its slots here other than @but.
    fn rival(&self, key: &I::Target, but: *const Slot<I>) -> Option<Rival> {
        let mut marked = false;
        for o in self.index(key).into_iter().map(|i| &self.slots[i]).filter(|&o| !core::ptr::eq(o, but)) {
            match o.load() {
                Contents::Item(x) if key_of(&x) == key => return Some(Rival::In),
                Contents::Unstable(x) if key_of(&x) == key => marked = true,
                _ => {}
            }
        }
        marked.then_some(Rival::Marked)
    }
}

/// Another of an item's key found, by an insert of it.
enum Rival {
    In,
    Marked,
}

/// Why an item isn't in yet: the newest table it found - its head, or null -
/// was full, or had no chain for it.
struct Full {
    newest:   *const Head,
    no_chain: bool,
}

impl<I: CuckooItem, S: CuckooHasher, const W: usize, M: Mode> Cuckoo<I, S, W, M> {
    /// The slots of its newest table: what it has room for as it is.
    pub fn capacity(&self) -> usize {
        self.tables(&read_lock()).last().map_or(0, |t| t.slots.len())
    }

    fn tables<'g>(&'g self, g: &'g ReadGuard) -> impl Iterator<Item = Table<'g, I, S, W>> + use<'g, I, S, W, M> {
        let mut oldest = self.tables.read_head(g).map(|(head, slots)| Table::new(head, slots, &self.faults));
        let mut prev: Option<Table<'g, I, S, W>> = None;
        // Not successors(), which loads a table's next as it hands the table
        // out: next is loaded only once the caller's done with the table, so
        // a slot it found retired was drained into one it's yet to see
        core::iter::from_fn(move || {
            let t = match prev.take() {
                Some(p) => p.next()?,
                None => oldest.take()?,
            };
            prev = Some(t);
            Some(t)
        })
    }

    pub(super) fn lookup<Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &ReadGuard) -> Option<I>
    where I::Target: Borrow<Q>
    {
        let mirror = self.mirror.load();
        let mirrored = key_slots::<W, S, Q>(mirror, key);

        let (head, slots) = self.tables.read_head(g)?;
        if let Probe::Hit(_, i) = Table::<I, S, W>::new(head, slots, &self.faults).probe(key, mirrored) {
            return Some(i);
        }
        self.tables(g).find_map(|t| self.find(&t, key, t.slots_for(key, mirror, mirrored))).map(|(_, i)| i)
    }

    /// @key's slot and item in @t, by a probe no move ran during and that
    /// met nothing unstable.
    fn find<'a, Q: ?Sized + Eq>(&self, t: &Table<'a, I, S, W>, key: &Q, slots: [usize; W]) -> Option<(&'a Slot<I>, I)>
    where I::Target: Borrow<Q>
    {
        loop {
            let moves = self.moves.load(Acquire);
            match t.probe(key, slots) {
                Probe::Hit(s, i) => return Some((s, i)),
                Probe::Unstable => {
                    saw!(self.faults, unstable);
                    core::hint::spin_loop();
                    continue;
                }
                Probe::Miss => {}
            }
            fence(Acquire);
            if self.moves.load(Relaxed) == moves {
                return None;
            }
            saw!(self.faults, retry);
        }
    }

    fn find_all<'g, Q: ?Sized + Hash + Eq>(&'g self, key: &Q, g: &'g ReadGuard) -> Option<(&'g Slot<I>, I)>
    where I::Target: Borrow<Q>
    {
        self.tables(g).find_map(|t| self.find(&t, key, t.index(key)))
    }

    /// With @nofail, growing the table can't fail: __GFP_NOFAIL.
    pub(super) fn put(&self, item: I, replace: bool, nofail: bool) -> Result<Option<I>, AllocError> {
        assert!(item.storable(), "cuckoo: an item that can't be in a table");

        loop {
            let full = match self.try_put(item, replace, &read_lock()) {
                Ok(old) => return Ok(old),
                Err(full) => full,
            };
            self.grow(full, nofail)?;
        }
    }

    fn try_put(&self, item: I, replace: bool, g: &ReadGuard) -> Result<Option<I>, Full> {
        let key = key_of(&item);

        loop {
            if let Some((s, old)) = self.find_all(key, g) {
                if !replace || s.cas(Contents::Item(old), Contents::Item(item)) {
                    return Ok(Some(old));
                }
                continue;
            }

            let Some(t) = self.tables(g).last() else {
                return Err(Full { newest: core::ptr::null(), no_chain: false });
            };
            if !fits::<W>(t.bits(), self.nr.load(Relaxed) + 1, &self.faults) {
                return Err(Full { newest: t.head, no_chain: false });
            }
            let chain = match t.find_path(key) {
                Ok(chain) => chain,
                Err(NotIn::NoChain) => return Err(Full { newest: t.head, no_chain: true }),
                Err(NotIn::Retired) if t.is_newest() => return Err(Full { newest: t.head, no_chain: false }),
                Err(_) => {
                    core::hint::spin_loop();
                    continue;
                }
            };

            self.faults.move_window();
            let _cpu = CpuGuard::new();
            let Ok(s) = t.claim_room(&chain, &self.moves) else {
                core::hint::spin_loop();
                continue;
            };
            // counted before it's in, so a remove never takes the count past it
            self.nr.fetch_add(1, Relaxed);
            s.store(Contents::Unstable(item));
            self.faults.move_window();
            fence(SeqCst);
            if self.settle(&t, s, item, g) {
                return Ok(None);
            }
            self.nr.fetch_sub(1, Relaxed);
        }
    }

    /// @s, one of @item's slots in @t, holds it marked, and a SeqCst fence is
    /// past: the bit cleared, and true - or @s emptied, and false, if a newer
    /// table's been linked, or @item's key is in an older table or another of
    /// its slots in @t. A marked one in an older table is waited out, as is one
    /// in @t if @s is the key's slot 0, which wins.
    fn settle(&self, t: &Table<'_, I, S, W>, s: &Slot<I>, item: I, g: &ReadGuard) -> bool {
        let key = key_of(&item);
        let first = core::ptr::eq(s, &t.slots[t.index(key)[0]]);

        let withdraw = || {
            s.store(Contents::Empty);
            false
        };

        loop {
            if !t.is_newest() {
                saw!(self.faults, stale);
                return withdraw();
            }
            // oldest to @t, and no further: waits go from newer to older,
            // never back
            let mut wait = false;
            for u in self.tables(g) {
                let own = core::ptr::eq(u.head, t.head);
                // a scan that missed a rival moved under it - its new slot
                // read before the fill, its old after the overwrite - sees the
                // move count change, as a lookup's probe does
                let rival = loop {
                    let moves = self.moves.load(Acquire);
                    let r = u.rival(key, s);
                    if r.is_some() {
                        break r;
                    }
                    fence(Acquire);
                    if self.moves.load(Relaxed) == moves {
                        break None;
                    }
                    saw!(self.faults, retry);
                };
                let lost = match rival {
                    Some(Rival::In) => true,
                    Some(Rival::Marked) if own && !first => true,
                    Some(Rival::Marked) => {
                        wait = true;
                        false
                    }
                    None => false,
                };
                if lost {
                    saw!(self.faults, withdrawn);
                    return withdraw();
                }
                if own {
                    break;
                }
            }
            if !wait {
                s.store(Contents::Item(item));
                return true;
            }
            core::hint::spin_loop();
        }
    }

    pub(super) fn take_if<Q: ?Sized + Hash + Eq>(&self, key: &Q, pred: impl Fn(&I) -> bool) -> Option<I>
    where I::Target: Borrow<Q>
    {
        let g = read_lock();
        loop {
            let (s, i) = self.find_all(key, &g).filter(|(_, i)| pred(i))?;
            if s.cas(Contents::Item(i), Contents::Empty) {
                self.nr.fetch_sub(1, Relaxed);
                return Some(i);
            }
        }
    }

    /// The oldest table. Not to be kept across a drain() or empty().
    fn locked<'a>(&'a self, g: &Held<'_>) -> Option<Table<'a, I, S, W>> {
        debug_assert!(core::ptr::eq(g.lock(), &self.lock));
        // SAFETY: we hold the rehash lock, and no replace() comes while the
        // view is in use
        unsafe { self.tables.get_head_locked() }
            .map(|(head, slots)| Table::new(head, slots, &self.faults))
    }

    fn tables_locked<'a>(&'a self, g: &Held<'_>) -> impl Iterator<Item = Table<'a, I, S, W>> + use<'a, I, S, W, M> {
        core::iter::successors(self.locked(g), Table::next)
    }

    /// What @full asked for, unless another rehash beat us to it: a newer
    /// table, and the older drained.
    fn grow(&self, full: Full, nofail: bool) -> Result<(), AllocError> {
        let g = self.lock.lock();
        let newest = self.tables_locked(&g).last();

        if newest.as_ref().map_or(core::ptr::null(), |t| t.head as *const Head) == full.newest {
            let nr = self.nr.load(Relaxed) + 1;
            let mut bits = newest.map_or(MIN_BITS, |t| t.bits() + full.no_chain as u32);
            while !fits::<W>(bits, nr, &self.faults) {
                bits += 1;
            }
            self.link(&g, bits, nofail)?;
            saw!(self.faults, rehash);
        }
        // an allocation failure leaves the rest to the next
        let _ = self.drain(&g);
        Ok(())
    }

    /// Past three quarters of what fits(), a rehash now.
    pub(super) fn grow_early(&self) -> Result<(), AllocError> {
        let roomy = |bits| {
            let nr = self.nr.load(Relaxed);
            fits::<W>(bits, nr + nr / 3, &self.faults)
        };
        // most often there's room: seen without the lock, which every caller
        // would otherwise take
        if self.tables(&read_lock()).last().is_none_or(|t| roomy(t.bits())) {
            return Ok(());
        }

        let g = self.lock.lock();
        let Some(newest) = self.tables_locked(&g).last() else { return Ok(()) };
        if roomy(newest.bits()) {
            return Ok(());
        }

        let mut bits = newest.bits() + 1;
        while !roomy(bits) {
            bits += 1;
        }
        self.link(&g, bits, false)?;
        saw!(self.faults, rehash);
        let _ = self.drain(&g);
        Ok(())
    }

    fn link(&self, g: &Held<'_>, bits: u32, nofail: bool) -> Result<(), AllocError> {
        let seeds: Seeds = core::array::from_fn(|_| random_u64());
        let head = Head { seeds, next: AtomicPtr::default() };
        let new = if nofail {
            RcuArray::<Slot<I>, Head>::alloc_nofail(1 << bits, head)
        } else {
            if self.faults.alloc_fail() {
                return Err(AllocError);
            }
            RcuArray::<Slot<I>, Head>::alloc(1 << bits, head)?
        };

        match self.tables_locked(g).last() {
            Some(t) => t.head.next.store(new.as_ptr(), Release),
            // SAFETY: we hold the rehash lock
            None => unsafe { self.tables.replace(new) },
        }
        fence(SeqCst);
        self.mirror.store(&seeds, bits);
        Ok(())
    }

    fn drain(&self, g: &Held<'_>) -> Result<(), AllocError> {
        while let Some(old) = self.locked(g) {
            let Some(next) = NonNull::new(old.head.next.load(Relaxed)) else { break };
            for s in old.slots {
                self.retire(g, s, true)?;
            }
            // SAFETY: we hold the rehash lock, and @old isn't used past this
            unsafe { self.tables.replace(next) }
        }
        Ok(())
    }

    /// @s, in a table with a newer one, retired: its item moved to the newest
    /// - or, unless @keep, dropped.
    fn retire(&self, g: &Held<'_>, s: &Slot<I>, keep: bool) -> Result<(), AllocError> {
        loop {
            // what's read of an entry is under RCU: a remove needs no lock
            let rcu = read_lock();
            let i = match s.load() {
                Contents::Retired => return Ok(()),
                Contents::Empty if s.cas(Contents::Empty, Contents::Retired) => return Ok(()),
                Contents::Item(i) if !keep => {
                    if s.cas(Contents::Item(i), Contents::Retired) {
                        self.nr.fetch_sub(1, Relaxed);
                        return Ok(());
                    }
                    continue;
                }
                Contents::Item(i) => i,
                _ => {
                    core::hint::spin_loop();
                    continue;
                }
            };

            let t = self.tables_locked(g).last().expect("cuckoo: draining with no newer table");
            let chain = match t.find_path(key_of(&i)) {
                Ok(chain) => chain,
                Err(NotIn::NoChain) => {
                    saw!(self.faults, grown);
                    drop(rcu);
                    self.link(g, t.bits() + 1, false)?;
                    continue;
                }
                Err(_) => {
                    core::hint::spin_loop();
                    continue;
                }
            };

            self.faults.move_window();
            let _cpu = CpuGuard::new();
            let Ok(d) = t.claim_room(&chain, &self.moves) else {
                core::hint::spin_loop();
                continue;
            };
            d.store(Contents::Unstable(i));
            self.faults.move_window();
            if s.cas(Contents::Item(i), Contents::Retired) {
                d.store(Contents::Item(i));
                return Ok(());
            }
            d.store(Contents::Empty);
        }
    }

    /// Every item dropped: each table's slots retired, so an insert still on
    /// one goes on to a newer one, then every table unlinked.
    pub(super) fn empty(&self, g: &Held<'_>) {
        for t in self.tables_locked(g) {
            for s in t.slots {
                let _ = self.retire(g, s, false);
            }
        }

        let mut next = self.locked(g).and_then(|t| NonNull::new(t.head.next.load(Relaxed)));
        // SAFETY: we hold the rehash lock
        unsafe { self.tables.clear() }
        while let Some(p) = next {
            // SAFETY: linked, so alloc()'s, and freed only here: by the time
            // it goes, so have the readers that could reach it
            unsafe {
                next = NonNull::new(RcuArray::<Slot<I>, Head>::parts(p).0.next.load(Relaxed));
                kvfree_rcu(p.as_ptr());
            }
        }
    }

    /// With no writer running.
    pub(super) fn check_invariant(&self, g: &Held<'_>) -> Result<usize, &'static str> {
        let _rcu = read_lock();
        let mut nr = 0;

        for t in self.tables_locked(g) {
            let newest = t.is_newest();
            for s in t.slots {
                let i = match s.load() {
                    Contents::Item(i) => i,
                    Contents::Empty => continue,
                    Contents::Retired if !newest => continue,
                    Contents::Retired => return Err("cuckoo: a retired slot in the newest table"),
                    _ => return Err("cuckoo: a claim or mark with no writer"),
                };
                match t.find_item(key_of(&i)) {
                    None => return Err("cuckoo: an item in a slot it doesn't hash to"),
                    Some(f) if !core::ptr::eq(f, s) => return Err("cuckoo: an item in two slots"),
                    Some(_) => nr += 1,
                }
                if self.tables_locked(g).any(|u| !core::ptr::eq(u.head, t.head) && u.find_item(key_of(&i)).is_some()) {
                    return Err("cuckoo: an item in two tables");
                }
            }
        }
        if nr != self.nr.load(Relaxed) {
            return Err("cuckoo: an item count that isn't the items");
        }
        Ok(nr)
    }
}

impl<I: CuckooItem, S, const W: usize, M: Mode> Drop for Cuckoo<I, S, W, M> {
    /// The oldest table is the RcuArray's to free; the newer ones linked from
    /// it are ours.
    fn drop(&mut self) {
        // SAFETY: &mut: no writer, and no reader
        let mut next = unsafe { self.tables.get_head_locked() }
            .and_then(|(head, _)| NonNull::new(head.next.load(Relaxed)));
        while let Some(p) = next {
            // SAFETY: linked, so alloc()'s, and nothing reaches it after this
            unsafe {
                next = NonNull::new(RcuArray::<Slot<I>, Head>::parts(p).0.next.load(Relaxed));
                RcuArray::<Slot<I>, Head>::free_now(p);
            }
        }
    }
}
