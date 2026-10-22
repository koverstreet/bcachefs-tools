// SPDX-License-Identifier: GPL-2.0

//! A cuckoo hash table looked up without a lock, under RCU: a set of u64
//! keys, or a map from u64 keys to entries that hold their own - generic over
//! its hash, SipHash-1-3 or hash_64()'s golden ratio, and its ways, two or
//! three. It replaces util/cuckoo.c once its users are Rust.
//!
//! # The buffer
//!
//! One RcuArray: the seeds, as its header, and 2^bits slots. A key has a slot
//! in each way, which the hash gives (CuckooHash); a slot holds an item
//! (CuckooItem) in one atomic word - a key, complemented, so the zeroes a
//! buffer is allocated as are empty and u64::MAX can't be one, or a pointer
//! to an entry, null for none. A lookup checks a slot per way: at most that
//! many cache misses.
//!
//! The hashes. SipHash-1-3, keyed by the seeds, gives a key's slots as if
//! independent, whatever structure the keys have: two from one hash - i, and
//! i ^ delta, delta odd, so each is the other's other - and for three ways a
//! third from a second hash. hash_64() of the key xored with a seed per way
//! is a multiply each, several times cheaper; but the ways are correlated, so
//! keys with structure - positions, inode numbers - can collide in every way
//! under every seed. It's for keys known to be spread, or where a lookup's
//! cost matters more than a rehash's.
//!
//! The ways. Two fill to about half the table, three to most of it, so the
//! table grows - a rehash, into a buffer twice the size with new seeds -
//! before an insert would take it past 45%, for two, short of where inserts
//! start to fail, or three quarters, for three. An insert that finds no room anyway rehashes with the item - new
//! seeds at that size, then the next size up - and one that can't allocate
//! fails with the table as it was. A table only shrinks by clear().
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

use arrayvec::ArrayVec;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize};
use core::sync::atomic::Ordering::{self, Acquire, Relaxed, Release};

use crate::c;
use crate::types::CacheAligned;
use crate::util::alloc::AllocError;
use crate::util::kernel::random_u64;
use crate::util::projections::Publish;
use crate::util::rcu::{read_lock, RcuArray, RcuElement, ReadGuard};
use crate::util::siphash::SipHash13;

/// The longest chain of moves an insert follows before it rehashes.
const MAX_KICKS: usize = 32;

/// An insert's chain of slots: u32s, as an RcuArray is at most INT_MAX bytes.
type Path = ArrayVec<u32, { MAX_KICKS + 1 }>;

/// The smallest table.
const MIN_BITS: u32 = 2;

/// A hash's seeds: SipHash's key and a second key's half, or one per way.
type Seeds = [u64; 3];

// ── The hashes ───────────────────────────────────────────────────────────

/// How a table finds a key's slots: see the top of the file.
pub trait CuckooHash {
    /// @key's slot in each of @W ways, in a table of 2^@bits. For zero @bits -
    /// a Mirror not yet written - any index, in the table or past it.
    fn slots<const W: usize>(seeds: &Seeds, bits: u32, key: u64) -> [usize; W];
}

pub struct SipHash;

impl CuckooHash for SipHash {
    fn slots<const W: usize>(seeds: &Seeds, bits: u32, key: u64) -> [usize; W] {
        let mask = (1usize << bits) - 1;
        let m = key.to_le_bytes();
        let h = SipHash13::hash(seeds[0], seeds[1], &m);
        let i = h as usize & mask;
        core::array::from_fn(|w| match w {
            0 => i,
            1 => i ^ (((h >> 32) as usize | 1) & mask),
            _ => SipHash13::hash(seeds[2], seeds[1], &m) as usize & mask,
        })
    }
}

pub struct GoldenRatio;

impl CuckooHash for GoldenRatio {
    fn slots<const W: usize>(seeds: &Seeds, bits: u32, key: u64) -> [usize; W] {
        core::array::from_fn(|w| {
            (key ^ seeds[w]).wrapping_mul(c::GOLDEN_RATIO_64).wrapping_shr(64 - bits) as usize
        })
    }
}

// ── What a table holds ───────────────────────────────────────────────────

/// What a table's slots hold, a word each.
///
/// # Safety
/// Atomic owns nothing, so a bytewise copy is another; all zeroes is empty;
/// load() gives back what store() stored.
pub unsafe trait CuckooItem: Copy {
    type Atomic: Send + Sync;

    fn key(self) -> u64;

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

    fn key(self) -> u64 {
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

/// An entry a map points to: its key, which doesn't change while it's in one.
pub trait CuckooEntry: Sync {
    fn key(&self) -> u64;
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
    /// @e stays valid, its key unchanged, for as long as it's in a table and
    /// a grace period after: removed, it's freed only after synchronize_rcu(),
    /// or by kfree_rcu().
    pub unsafe fn new(e: NonNull<E>) -> Self {
        Self(e)
    }

    pub fn as_ptr(self) -> NonNull<E> {
        self.0
    }
}

// SAFETY: an AtomicPtr, null for empty
unsafe impl<E: CuckooEntry> CuckooItem for Entry<E> {
    type Atomic = AtomicPtr<E>;

    fn key(self) -> u64 {
        // SAFETY: valid while in a table and a grace period after (new()): a
        // reader had it from a slot in a read-side section, the writer from a
        // slot or new()
        unsafe { self.0.as_ref() }.key()
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
    /// @key's item, if this holds it.
    fn get(&self, key: u64) -> Option<I> {
        I::load(&self.0, Acquire).filter(|i| i.key() == key)
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
fn fits<const W: usize>(bits: u32, nr: usize) -> bool {
    let slots = 1usize << bits;
    if W == 2 {
        nr * 20 <= slots * 9
    } else {
        nr * 4 <= slots * 3
    }
}

/// A buffer's seeds and slots, as a reader - or the writer - has them.
struct Table<'a, I: CuckooItem, H, const W: usize> {
    seeds: &'a Seeds,
    slots: &'a [Slot<I>],
    hash:  PhantomData<fn() -> H>,
}

impl<'a, I: CuckooItem, H: CuckooHash, const W: usize> Table<'a, I, H, W> {
    fn new(seeds: &'a Seeds, slots: &'a [Slot<I>]) -> Self {
        Self { seeds, slots, hash: PhantomData }
    }

    fn bits(&self) -> u32 {
        self.slots.len().trailing_zeros()
    }

    fn index(&self, key: u64) -> [usize; W] {
        H::slots(self.seeds, self.bits(), key)
    }

    /// @key's item, from the slots this buffer's seeds name: for a lookup
    /// whose Mirror was another buffer's.
    #[cold]
    fn get_raced(&self, key: u64) -> Option<I> {
        self.index(key).into_iter().find_map(|i| self.slots[i].get(key))
    }

    /// Writer: the slot holding @key.
    fn find_locked(&self, key: u64) -> Option<&'a Slot<I>> {
        self.index(key).into_iter()
            .map(|i| &self.slots[i])
            .find(|s| s.load_locked().is_some_and(|i| i.key() == key))
    }

    /// Writer: what the table holds.
    fn iter_locked(&self) -> impl Iterator<Item = I> + use<'a, I, H, W> {
        self.slots.iter().filter_map(Slot::load_locked)
    }

    /// Where @key, which isn't in the table, can go, into @path: one of its
    /// slots, empty, or a chain from one - walk(). False if there's none.
    fn find_path(&self, key: u64, path: &mut Path) -> bool {
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

            let mut next = None;
            for o in self.index(item.key()) {
                if path.contains(&(o as u32)) {
                    continue;
                }
                next = Some(o);
                if self.slots[o].load_locked().is_none() {
                    break;
                }
            }
            let Some(n) = next else { return false };
            s = n;
        }
    }

    /// Insert @item, whose key isn't in the table, along find_path()'s chain,
    /// counting each move in @moves: false, and nothing moved, if there's no
    /// chain.
    fn insert(&self, item: I, moves: &AtomicU64) -> bool {
        let mut path = Path::new();
        if !self.find_path(item.key(), &mut path) {
            return false;
        }

        for w in path.windows(2).rev() {
            let (from, to) = (&self.slots[w[0] as usize], &self.slots[w[1] as usize]);
            to.set(from.load_locked().expect("on the path: occupied"));
            moves.fetch_add(1, Release);
            from.clear();
        }
        self.slots[path[0] as usize].set(item);
        true
    }
}

/// See the top of the file: of @I - u64 keys, or Entry<E> - hashed by @H, in
/// @W ways, two or three. Zeroed, it's empty, with no buffer: the first
/// insert allocates one.
///
/// repr(C): what a hit reads, which only a rehash writes, apart from what the
/// writer writes on every insert.
#[repr(C)]
pub struct Cuckoo<I: CuckooItem = u64, H: CuckooHash = SipHash, const W: usize = 2> {
    mirror:  Mirror,
    table:   RcuArray<Slot<I>, Seeds>,
    _writer: CacheAligned,
    /// Items moved by inserts, ever: read only by a lookup that misses.
    moves:   AtomicU64,
    /// Items, for when to grow.
    nr:      AtomicUsize,
    hash:    PhantomData<fn() -> H>,
}

// SAFETY: atomics, an RcuArray, no bytes and a PhantomData: all valid zeroed
unsafe impl<I: CuckooItem, H: CuckooHash, const W: usize> ::pin_init::Zeroable for Cuckoo<I, H, W> {}

impl<I: CuckooItem, H: CuckooHash, const W: usize> Default for Cuckoo<I, H, W> {
    fn default() -> Self {
        ::pin_init::Zeroable::zeroed()
    }
}

impl<I: CuckooItem, H: CuckooHash, const W: usize> Cuckoo<I, H, W> {
    const WAYS: () = assert!(W == 2 || W == 3, "cuckoo: two ways or three");

    /// @key's item, in a read-side section of its own.
    pub fn get(&self, key: u64) -> Option<I> {
        self.get_rcu(key, &read_lock())
    }

    pub fn contains(&self, key: u64) -> bool {
        self.get(key).is_some()
    }

    /// @key's item, under @g's section.
    pub fn get_rcu(&self, key: u64, g: &ReadGuard) -> Option<I> {
        let () = Self::WAYS;
        let mirror = self.mirror.load();
        let slots = H::slots::<W>(&mirror.0, mirror.1, key);

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
        }
    }

    /// @key's item, from @slots - its, under @mirror, which may be another
    /// buffer's - or from the buffer's own if the mirror was stale.
    fn probe(&self, key: u64, g: &ReadGuard, mirror: (Seeds, u32), slots: [usize; W]) -> Option<I> {
        let (seeds, buf) = self.table.read_head(g)?;
        let t = Table::<I, H, W>::new(seeds, buf);

        let hit = slots.into_iter().find_map(|i| buf.get(i)?.get(key));
        hit.or_else(|| (mirror != (*seeds, t.bits())).then(|| t.get_raced(key)).flatten())
    }

    /// The writer's methods, with the table to ourselves.
    pub fn get_mut(&mut self) -> &CuckooWriter<I, H, W> {
        // SAFETY: laid out as Self (Publish), and exclusive: the one writer
        unsafe { &*core::ptr::from_mut(self).cast::<CuckooWriter<I, H, W>>() }
    }
}

impl<E: CuckooEntry, H: CuckooHash, const W: usize> Cuckoo<Entry<E>, H, W> {
    /// @key's entry, for as long as @g's read-side section.
    pub fn get_entry<'g>(&self, key: u64, g: &'g ReadGuard) -> Option<&'g E> {
        // SAFETY: in the table during @g's section, so valid until it ends
        // (Entry::new())
        self.get_rcu(key, g).map(|e| unsafe { e.0.as_ref() })
    }
}

// SAFETY: CuckooWriter is a Cuckoo; its writes are the ones at the top of the
// file, which a reader may overlap
unsafe impl<I: CuckooItem, H: CuckooHash, const W: usize> Publish for Cuckoo<I, H, W> {
    type Writer = CuckooWriter<I, H, W>;
}

/// A Cuckoo, as its one writer has it: the holder of the lock it's published
/// under, or of a &mut of it.
#[repr(transparent)]
pub struct CuckooWriter<I: CuckooItem = u64, H: CuckooHash = SipHash, const W: usize = 2>(Cuckoo<I, H, W>);

impl<I: CuckooItem, H: CuckooHash, const W: usize> core::ops::Deref for CuckooWriter<I, H, W> {
    type Target = Cuckoo<I, H, W>;

    fn deref(&self) -> &Cuckoo<I, H, W> {
        &self.0
    }
}

impl<I: CuckooItem, H: CuckooHash, const W: usize> CuckooWriter<I, H, W> {
    /// Not to be kept past a rehash() or clear(): see the top of the file.
    fn locked(&self) -> Option<Table<'_, I, H, W>> {
        // SAFETY: we're the writer - had only through a lock's guard or a
        // &mut - and no replace() comes while the view is in use
        unsafe { self.0.table.get_head_locked() }
            .map(|(seeds, slots)| Table::new(seeds, slots))
    }

    /// @item, in place of what had its key, if anything: that. Fails, with
    /// the table unchanged, only if it can't allocate.
    pub fn insert(&self, item: I) -> Result<Option<I>, AllocError> {
        let () = Cuckoo::<I, H, W>::WAYS;
        assert!(item.storable(), "cuckoo: {:#x} can't be in a table", item.key());

        let t = self.locked();
        if let Some(s) = t.as_ref().and_then(|t| t.find_locked(item.key())) {
            let old = s.load_locked().expect("found: occupied");
            s.set(item);
            return Ok(Some(old));
        }

        let nr = self.0.nr.load(Relaxed) + 1;
        let room = t.filter(|t| fits::<W>(t.bits(), nr));
        if !room.is_some_and(|t| t.insert(item, &self.0.moves)) {
            return self.rehash(item).map(|()| None);
        }
        self.0.nr.store(nr, Relaxed);
        Ok(None)
    }

    /// Take @key out: its item, if it was in.
    pub fn remove(&self, key: u64) -> Option<I> {
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

    /// A new buffer with every item, and @item: see the top of the file. On
    /// an error the table is unchanged.
    fn rehash(&self, item: I) -> Result<(), AllocError> {
        let old = self.locked();
        let nr = self.0.nr.load(Relaxed) + 1;

        let mut bits = old.as_ref().map_or(MIN_BITS, Table::bits);
        while !fits::<W>(bits, nr) {
            bits += 1;
        }

        // The new buffer's moves are before anyone can see it: not counted
        let moves = AtomicU64::new(0);

        loop {
            for _ in 0..3 {
                let seeds: Seeds = core::array::from_fn(|_| random_u64());
                // SAFETY: we're the writer; @old's slots don't change under
                // us, and nothing reads them past the replace()
                let fit = unsafe {
                    self.0.table.rebuild_head(1 << bits, seeds, |slots| {
                        let t = Table::<I, H, W>::new(&seeds, slots);
                        old.iter().flat_map(Table::iter_locked).chain([item])
                            .all(|i| t.insert(i, &moves))
                            .then_some(()).ok_or(())
                    })
                }?;
                if fit.is_ok() {
                    self.0.mirror.store(&seeds, bits);
                    self.0.nr.store(nr, Relaxed);
                    return Ok(());
                }
            }
            bits += 1;
        }
    }
}
