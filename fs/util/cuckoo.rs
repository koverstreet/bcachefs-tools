// SPDX-License-Identifier: GPL-2.0

//! A cuckoo hash table looked up without a lock, under RCU: a set of u64s, or
//! a map to entries that hold their own keys - in two ways or three, and
//! hashed by a BuildHasher, as std's HashMap is: SipHash-1-3, by default, or
//! hash_64()'s golden ratio (util/hash.rs) where a lookup's cost matters more
//! than keys with structure colliding.
//!
//! Keys are std's: an item - the u64, or the entry it points to - is Hash +
//! Eq, and a lookup takes anything it Borrows, as HashSet::get() does; an
//! entry's Hash and Eq are its key's. An entry the map doesn't own can hold
//! its own place in it, and its key - a Registration - so that dropping the
//! entry takes it out.
//!
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
//! # Readers and the writer
//!
//! Writers are serialized by the lock that publishes the table (Publish), or
//! by a &mut of it. A reader sees a key a write isn't touching, always:
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
//! The writer's view of the buffer, locked(), is a borrow the types don't
//! end: rehash() and clear() free the buffer after a grace period the writer
//! isn't in, and take &self like everything else. So this file keeps to it:
//! rehash() reads the old buffer only in its fill, before the replace, and
//! insert() takes no view across one.
//!
//! # Faults
//!
//! With CONFIG_BCACHEFS_TESTS a table has Faults, given by the torture test
//! (debug/tests/cuckoo.rs), for what load alone rarely reaches: windows
//! inside moves and between a lookup's slots, failed allocations, a packed
//! table - and Seen, counts of how often each rare path ran. Without, Faults
//! is zero sized and every hook a constant.

use arrayvec::ArrayVec;
use core::borrow::Borrow;
use core::hash::{BuildHasher, Hash};
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize};
use core::sync::atomic::Ordering::{self, Acquire, Relaxed, Release};

use crate::types::CacheAligned;
use crate::util::alloc::AllocError;
use crate::util::kernel::random_u64;
use crate::util::projections::Publish;
use crate::util::rcu::{read_lock, RcuArray, RcuElement, ReadGuard};
use crate::util::siphash::SipHash13Key;

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
type Seeds = [u64; 3];

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

// ── What a table holds ───────────────────────────────────────────────────

/// What a table's slots hold, a word each.
///
/// # Safety
/// Atomic owns nothing, so a bytewise copy is another; all zeroes is empty;
/// load() gives back what store() stored.
pub unsafe trait CuckooItem: Copy {
    type Atomic: Send + Sync;

    /// What it is, or points to: hashed and compared as its key.
    type Target: ?Sized + Hash + Eq;

    /// # Safety
    /// The item's in a table: read from a slot in a read-side section, or
    /// by the writer - an entry's valid until a grace period after it's out.
    unsafe fn target(&self) -> &Self::Target;

    /// Whether it can go in a table at all.
    fn storable(self) -> bool {
        true
    }

    fn load(a: &Self::Atomic, o: Ordering) -> Option<Self>;
    fn store(a: &Self::Atomic, v: Option<Self>, o: Ordering);
}

// SAFETY: an AtomicU64, holding the key complemented: zero is empty
unsafe impl CuckooItem for u64 {
    type Atomic = AtomicU64;
    type Target = u64;

    unsafe fn target(&self) -> &u64 {
        self
    }

    fn storable(self) -> bool {
        self != u64::MAX
    }

    fn load(a: &AtomicU64, o: Ordering) -> Option<u64> {
        Some(a.load(o)).filter(|&k| k != 0).map(|k| !k)
    }

    fn store(a: &AtomicU64, v: Option<u64>, o: Ordering) {
        a.store(v.map_or(0, |k| !k), o)
    }
}

/// A pointer to an entry, as a map holds it.
pub struct Entry<E>(NonNull<E>);

impl<E> Clone for Entry<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for Entry<E> {}

// SAFETY: a pointer to a Sync entry, which any thread may look at
unsafe impl<E: Sync> Send for Entry<E> {}
unsafe impl<E: Sync> Sync for Entry<E> {}

impl<E> Entry<E> {
    /// # Safety
    /// @e stays valid, its key - what its Hash and Eq see - unchanged, for as
    /// long as it's in a table and a grace period after: removed, it's freed
    /// only after synchronize_rcu(), or by kfree_rcu() - or at once, from a
    /// table no other thread reads.
    pub unsafe fn new(e: NonNull<E>) -> Self {
        Self(e)
    }

    pub fn as_ptr(self) -> NonNull<E> {
        self.0
    }
}

// SAFETY: an AtomicPtr, null for empty
unsafe impl<E: Hash + Eq + Sync> CuckooItem for Entry<E> {
    type Atomic = AtomicPtr<E>;
    type Target = E;

    unsafe fn target(&self) -> &E {
        // SAFETY: the caller's, and new()'s
        unsafe { self.0.as_ref() }
    }

    fn load(a: &AtomicPtr<E>, o: Ordering) -> Option<Self> {
        NonNull::new(a.load(o)).map(Self)
    }

    fn store(a: &AtomicPtr<E>, v: Option<Self>, o: Ordering) {
        a.store(v.map_or(core::ptr::null_mut(), |e| e.0.as_ptr()), o)
    }
}

#[repr(transparent)]
struct Slot<I: CuckooItem>(I::Atomic);

// SAFETY: CuckooItem's: owning nothing, and all zeroes empty
unsafe impl<I: CuckooItem> RcuElement for Slot<I> {}

impl<I: CuckooItem> Slot<I> {
    /// A reader's view.
    fn load(&self) -> Option<I> {
        I::load(&self.0, Acquire)
    }

    /// The writer's view.
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

/// The published buffer's seeds and bits - or another buffer's, if a rehash
/// raced, naming the wrong slots or ones past the end.
struct Mirror {
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
struct Table<'a, I: CuckooItem, S, const W: usize> {
    seeds:  Seeds,
    slots:  &'a [Slot<I>],
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

    fn key<'i>(&self, i: &'i I) -> &'i I::Target {
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
    fn find_locked<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<&'a Slot<I>>
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

/// See the top of the file: of @I - u64s, or Entry<E> - hashed by @S, in @W
/// ways, two or three. Zeroed, it's empty, with no buffer: the first insert
/// allocates one.
///
/// repr(C): what a hit reads, which only a rehash writes - and the faults, in
/// a test - apart from what the writer writes on every insert.
#[repr(C)]
pub struct Cuckoo<I: CuckooItem = u64, S = SipHash13Key, const W: usize = 2> {
    mirror:  Mirror,
    table:   RcuArray<Slot<I>, Seeds>,
    faults:  Faults,
    _writer: CacheAligned,
    /// Items moved by inserts, ever: read only by a lookup that misses.
    moves:   AtomicU64,
    /// Items, for when to grow.
    nr:      AtomicUsize,
    hash:    PhantomData<fn() -> S>,
}

// SAFETY: atomics, an RcuArray, no bytes, Faults' integers and atomics, and a
// PhantomData: all valid zeroed
unsafe impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize> ::pin_init::Zeroable for Cuckoo<I, S, W> {}

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize> Default for Cuckoo<I, S, W> {
    fn default() -> Self {
        ::pin_init::Zeroable::zeroed()
    }
}

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize> Cuckoo<I, S, W> {
    const WAYS: () = assert!(W == 2 || W == 3, "cuckoo: two ways or three");

    /// @key's item, in a read-side section of its own.
    pub fn get<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<I>
    where I::Target: Borrow<Q>
    {
        self.get_rcu(key, &read_lock())
    }

    pub fn contains<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> bool
    where I::Target: Borrow<Q>
    {
        self.get(key).is_some()
    }

    /// @key's item, under @g's section.
    pub fn get_rcu<Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &ReadGuard) -> Option<I>
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

    /// An empty table, with @faults injected.
    #[cfg(CONFIG_BCACHEFS_TESTS)]
    pub fn with_faults(faults: Faults) -> Self {
        Self { faults, ..Self::default() }
    }

    #[cfg(CONFIG_BCACHEFS_TESTS)]
    pub fn faults(&self) -> &Faults {
        &self.faults
    }

    #[cfg(CONFIG_BCACHEFS_TESTS)]
    pub fn moved(&self) -> u64 {
        self.moves.load(Relaxed)
    }

    /// The writer's methods, with the table to ourselves.
    pub fn get_mut(&mut self) -> &CuckooWriter<I, S, W> {
        // SAFETY: laid out as Self (Publish), and exclusive: the one writer
        unsafe { &*core::ptr::from_mut(self).cast::<CuckooWriter<I, S, W>>() }
    }
}

impl<E: Hash + Eq + Sync, S: BuildHasher + From<[u64; 2]>, const W: usize> Cuckoo<Entry<E>, S, W> {
    /// @key's entry, for as long as @g's read-side section.
    pub fn get_entry<'g, Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &'g ReadGuard) -> Option<&'g E>
    where E: Borrow<Q>
    {
        // SAFETY: in the table during @g's section, so valid until it ends
        // (Entry::new())
        self.get_rcu(key, g).map(|e| unsafe { e.0.as_ref() })
    }
}

// SAFETY: CuckooWriter is a Cuckoo; its writes are the ones at the top of the
// file, which a reader may overlap
unsafe impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize> Publish for Cuckoo<I, S, W> {
    type Writer = CuckooWriter<I, S, W>;
}

/// A Cuckoo, as its one writer has it: the holder of the lock it's published
/// under, or of a &mut of it. Not Sync: shared with another thread, it would
/// be two writers - and one's rehash() frees the buffer the other's locked()
/// is looking at.
#[repr(transparent)]
pub struct CuckooWriter<I: CuckooItem = u64, S = SipHash13Key, const W: usize = 2>(
    Cuckoo<I, S, W>,
    PhantomData<*const ()>,
);

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize> core::ops::Deref for CuckooWriter<I, S, W> {
    type Target = Cuckoo<I, S, W>;

    fn deref(&self) -> &Cuckoo<I, S, W> {
        &self.0
    }
}

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize> CuckooWriter<I, S, W> {
    /// Not to be kept past a rehash() or clear(): see the top of the file.
    fn locked(&self) -> Option<Table<'_, I, S, W>> {
        // SAFETY: we're the writer - had only through a lock's guard or a
        // &mut - and no replace() comes while the view is in use
        unsafe { self.0.table.get_head_locked() }
            .map(|(seeds, slots)| Table::new(*seeds, slots, &self.0.faults))
    }

    /// @item, in place of what had its key, if anything: that. Fails, with
    /// the table unchanged, only if it can't allocate.
    pub fn insert(&self, item: I) -> Result<Option<I>, AllocError> {
        let () = Cuckoo::<I, S, W>::WAYS;
        assert!(item.storable(), "cuckoo: an item that can't be in a table");

        let t = self.locked();
        if let Some(s) = t.as_ref().and_then(|t| t.find_locked(t.key(&item))) {
            let old = s.load_locked().expect("found: occupied");
            s.set(item);
            return Ok(Some(old));
        }

        let nr = self.0.nr.load(Relaxed) + 1;
        let room = t.filter(|t| fits::<W>(t.bits(), nr, &self.0.faults));
        if !room.is_some_and(|t| t.insert(item, &self.0.moves)) {
            return self.rehash(item).map(|()| None);
        }
        self.0.nr.store(nr, Relaxed);
        Ok(None)
    }

    /// Take @key out: its item, if it was in.
    pub fn remove<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<I>
    where I::Target: Borrow<Q>
    {
        let s = self.locked()?.find_locked(key)?;
        let old = s.load_locked();
        s.clear();
        self.0.nr.fetch_sub(1, Relaxed);
        old
    }

    /// Empty it, freeing its buffer after a grace period.
    pub fn clear(&self) {
        // SAFETY: we're the writer
        unsafe { self.0.table.clear() }
        self.0.nr.store(0, Relaxed);
    }

    /// The table's invariant, checked - every item in one of its slots, and in
    /// only one, and as many as it counts: how many, or what's broken. For
    /// tests, and debugging: it walks every slot.
    pub fn check(&self) -> Result<usize, &'static str> {
        let Some(t) = self.locked() else { return Ok(0) };
        let mut nr = 0;

        for s in t.slots {
            let Some(i) = s.load_locked() else { continue };
            match t.find_locked(t.key(&i)) {
                None => return Err("cuckoo: an item in a slot it doesn't hash to"),
                Some(f) if !core::ptr::eq(f, s) => return Err("cuckoo: an item in two slots"),
                Some(_) => nr += 1,
            }
        }
        if nr != self.0.nr.load(Relaxed) {
            return Err("cuckoo: an item count that isn't the items");
        }
        Ok(nr)
    }

    /// A new buffer with every item, and @item: see the top of the file. On
    /// an error the table is unchanged.
    fn rehash(&self, item: I) -> Result<(), AllocError> {
        let old = self.locked();
        let nr = self.0.nr.load(Relaxed) + 1;

        let faults = &self.0.faults;
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
                // SAFETY: we're the writer; @old's slots don't change under
                // us, and nothing reads them past the replace()
                let fit = unsafe {
                    self.0.table.rebuild_head(1 << bits, seeds, |slots| {
                        let t = Table::<I, S, W>::new(seeds, slots, &unpublished);
                        old.iter().flat_map(Table::iter_locked).chain([item])
                            .all(|i| t.insert(i, &moves))
                            .then_some(()).ok_or(())
                    })
                }?;
                if fit.is_ok() {
                    saw!(faults, rehash);
                    self.0.mirror.store(&seeds, bits);
                    self.0.nr.store(nr, Relaxed);
                    return Ok(());
                }
            }
            saw!(faults, grown);
            bits += 1;
        }
    }
}

// ── An entry's own registration ──────────────────────────────────────────

/// An entry's place in a map, and its key, held by the entry: dropping it
/// takes the entry out. A reference each way, owning neither - the table
/// points to the entry, the entry to the table - so the entry is pinned while
/// it's in, and leaves as it's dropped. The entry's Hash, Eq and Borrow<K>
/// are key()'s, which nothing can change.
///
/// The table's a pointer, not a borrow: a borrow's lifetime would be in the
/// entry's type, so in the table's, and a table can't be borrowed for a
/// lifetime its own type names - it has drop glue.
pub struct Registration<K, E, S = SipHash13Key, const W: usize = 2>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{
    table: NonNull<CuckooWriter<Entry<E>, S, W>>,
    entry: NonNull<E>,
    key:   K,
}

// SAFETY: shared, it gives out only the key; @entry is compared, never
// followed. Not Send: its drop is a write, the writer's.
unsafe impl<K, E, S, const W: usize> Sync for Registration<K, E, S, W>
where K: Hash + Eq + Sync, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{}

impl<K, E, S, const W: usize> Registration<K, E, S, W>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{
    /// @e, in @table under @key - which mustn't be in it - until this is
    /// dropped: made in place, as a field of @e, so the insert hashes @e
    /// through it. On an error the table is unchanged.
    ///
    /// # Safety
    /// Entry::new()'s, but for the key, which is this one's; @table outlives
    /// this, which is dropped by its writer - the lock's holder, or the &mut's;
    /// and @e is the entry this is a field of, being pin-initialized -
    /// pin-init's &this - with this its first field, so dropped first, and
    /// initialized last.
    pub unsafe fn new<'w>(table: &'w CuckooWriter<Entry<E>, S, W>, key: K, e: NonNull<E>)
        -> impl ::pin_init::PinInit<Self, AllocError> + use<'w, K, E, S, W>
    {
        let init = move |slot: *mut Self| {
            assert!(!table.locked().is_some_and(|t| t.find_locked(&key).is_some()),
                    "cuckoo: registering a key that's in already");
            // SAFETY: @slot is ours to write, pin-init's
            unsafe { slot.write(Self { table: NonNull::from(table), entry: e, key }) };
            // SAFETY: the caller's
            let ret = table.insert(unsafe { Entry::new(e) }).map(|_| ());
            if ret.is_err() {
                // SAFETY: just written; its drop finds nothing to take out
                unsafe { core::ptr::drop_in_place(slot) };
            }
            ret
        };
        // SAFETY: @slot is initialized on Ok, and not on Err; nothing moves it
        unsafe { ::pin_init::pin_init_from_closure(init) }
    }

    pub fn key(&self) -> &K {
        &self.key
    }
}

impl<K, E, S, const W: usize> Drop for Registration<K, E, S, W>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{
    /// Out, if the slot is still this entry's: an insert() of its key may
    /// have put another in its place.
    fn drop(&mut self) {
        // SAFETY: it outlives us, and we're its writer (new())
        let table = unsafe { self.table.as_ref() };
        let Some(s) = table.locked().and_then(|t| t.find_locked(&self.key)) else { return };
        if s.load_locked().is_some_and(|i| i.0 == self.entry) {
            s.clear();
            table.0.nr.fetch_sub(1, Relaxed);
        }
    }
}

// ── Faults, for the torture test ─────────────────────────────────────────

/// What the torture test injects into one table.
#[cfg(CONFIG_BCACHEFS_TESTS)]
#[derive(Default)]
pub struct Faults {
    /// Spins inside each move, between its copy, its count and its emptying
    /// - and a yield, the writer off CPU mid-chain: the writer must hold a
    /// lock it can sleep under.
    pub move_window: u32,
    /// Spins in a lookup before each slot it reads.
    pub read_window: u32,
    /// One in this many buffer allocations fails (0: none).
    pub alloc_fail:  u32,
    /// The table grows only when it's full: inserts walk long chains, and
    /// rehash to find room.
    pub dense:       bool,
    pub seen:        Seen,
}

/// How often each rare path ran: that a test reached them.
#[cfg(CONFIG_BCACHEFS_TESTS)]
#[derive(Default)]
pub struct Seen {
    /// Lookups that missed and retried: a move was counted during them.
    pub retry:  AtomicU64,
    /// Lookups that missed and looked again with the buffer's own seeds.
    pub raced:  AtomicU64,
    pub rehash: AtomicU64,
    /// Rehashes whose seeds didn't fit at a size, trying a bigger table.
    pub grown:  AtomicU64,
}

#[cfg(CONFIG_BCACHEFS_TESTS)]
fn spins(n: u32) {
    (0..n).for_each(|_| core::hint::spin_loop());
}

#[cfg(CONFIG_BCACHEFS_TESTS)]
impl Faults {
    fn move_window(&self) {
        if self.move_window != 0 {
            spins(self.move_window);
            crate::util::kernel::cond_resched();
        }
    }

    fn read_window(&self) { spins(self.read_window) }

    fn alloc_fail(&self) -> bool {
        self.alloc_fail != 0 && crate::util::kernel::random_u64_below(self.alloc_fail.into()) == 0
    }

    fn dense(&self) -> bool { self.dense }
}

#[cfg(not(CONFIG_BCACHEFS_TESTS))]
#[derive(Default)]
pub struct Faults;

#[cfg(not(CONFIG_BCACHEFS_TESTS))]
impl Faults {
    fn move_window(&self) {}
    fn read_window(&self) {}
    fn alloc_fail(&self) -> bool { false }
    fn dense(&self) -> bool { false }
}
