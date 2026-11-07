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

mod faults;
mod table;

#[cfg(CONFIG_BCACHEFS_TESTS)]
pub use faults::Seen;
pub use faults::Faults;

use core::borrow::Borrow;
use core::hash::{BuildHasher, Hash};
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize};
use core::sync::atomic::Ordering::{self, Relaxed};

use crate::locking::{ByteLock, ExclusiveGuard, Locked};
use crate::types::CacheAligned;
use crate::util::alloc::AllocError;
use crate::util::backref::BackRef;
use crate::util::rcu::{read_lock, Detach, DetachMember, RcuArray, RcuMember, ReadGuard};
use crate::util::siphash::SipHash13Key;
use table::{Mirror, Seeds, Slot};

// ── What a table holds ───────────────────────────────────────────────────

/// What a table's slots hold, a word each.
///
/// XXX: not cuckoo's. Atomic, load() and store() are util/projections'
/// AtomicWord again - with orderings, and None for empty: an empty niche
/// (Option<NonNull<E>>, NonZeroU64) would retire the complemented u64, and
/// storable() with it. And target() is Borrow, but for an entry's deref
/// being unsafe - which is the RCU types' to say: see Entry.
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
///
/// XXX: not cuckoo's. A pointer to what's freed only after a grace period
/// is an RCU type - the kernel's RcuBox/ForeignOwnableRcu, ours to have -
/// and new()'s contract is what that type would make its own.
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

/// Who looks a table up: any thread, under RCU - Shared - or only its
/// owner's - Private, and !Sync, so no other can. An entry a Shared table's
/// readers may have found stays valid a grace period after it's out, so its
/// Registration carries an RcuMember: what it's in is freed a grace period
/// on. A Private table's entries may go at once.
pub trait Mode {
    /// What a Registration in such a table carries
    type Member: Default;
    /// What the table does: whether it's Sync
    type Marker;
    /// Read by other threads, under RCU
    const SHARED: bool;
}

pub struct Shared;
pub struct Private;

impl Mode for Shared {
    type Member = RcuMember;
    type Marker = ();
    const SHARED: bool = true;
}

impl Mode for Private {
    type Member = ();
    type Marker = core::cell::Cell<()>;
    const SHARED: bool = false;
}

/// See the top of the file: of @I - u64s, or Entry<E> - hashed by @S, in @W
/// ways, two or three, read as @M says. Zeroed, it's empty, with no buffer:
/// the first insert allocates one.
///
/// repr(C): what a hit reads, which only a rehash writes - and the faults, in
/// a test - apart from what writers write on every insert.
#[repr(C)]
pub struct Cuckoo<I: CuckooItem = u64, S = SipHash13Key, const W: usize = 2, M: Mode = Shared> {
    mirror:  Mirror,
    table:   RcuArray<Slot<I>, Seeds>,
    faults:  Faults,
    _writer: CacheAligned,
    /// The writers'
    lock:    Locked<ByteLock>,
    /// Items moved by inserts, ever: read only by a lookup that misses.
    moves:   AtomicU64,
    /// Items, for when to grow.
    nr:      AtomicUsize,
    hash:    PhantomData<fn() -> S>,
    _mode:   PhantomData<M::Marker>,
}

/// The writers' lock, held.
type Held<'a> = ExclusiveGuard<'a, ByteLock>;

// SAFETY: atomics, an RcuArray, no bytes, an unlocked ByteLock, Faults'
// integers and atomics, and PhantomDatas: all valid zeroed
unsafe impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize, M: Mode> ::pin_init::Zeroable for Cuckoo<I, S, W, M> {}

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize, M: Mode> Default for Cuckoo<I, S, W, M> {
    fn default() -> Self {
        ::pin_init::Zeroable::zeroed()
    }
}

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize, M: Mode> Cuckoo<I, S, W, M> {
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

    pub fn len(&self) -> usize {
        self.nr.load(Relaxed)
    }

    /// @key's item, under @g's section.
    pub fn get_rcu<Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &ReadGuard) -> Option<I>
    where I::Target: Borrow<Q>
    {
        self.lookup(key, g)
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
}

impl<E: Hash + Eq + Sync, S: BuildHasher + From<[u64; 2]>, const W: usize, M: Mode> Cuckoo<Entry<E>, S, W, M> {
    /// @key's entry, for as long as @g's read-side section.
    pub fn get_entry<'g, Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &'g ReadGuard) -> Option<&'g E>
    where E: Borrow<Q>
    {
        // SAFETY: in the table during @g's section, so valid until it ends
        // (Entry::new())
        self.get_rcu(key, g).map(|e| unsafe { e.0.as_ref() })
    }
}

// ── Writers ──────────────────────────────────────────────────────────────

impl<I: CuckooItem, S: BuildHasher + From<[u64; 2]>, const W: usize, M: Mode> Cuckoo<I, S, W, M> {
    /// @item, in place of what had its key, if anything: that. Fails, with
    /// the table unchanged, only if it can't allocate.
    pub fn insert(&self, item: I) -> Result<Option<I>, AllocError> {
        self.put(&self.lock.lock(), item, true)
    }

    /// @item, if nothing has its key: else what does, as the error, and @item
    /// isn't in. Fails, with the table unchanged, only if it can't allocate.
    pub fn insert_new(&self, item: I) -> Result<Result<(), I>, AllocError> {
        Ok(self.put(&self.lock.lock(), item, false)?.map_or(Ok(()), Err))
    }

    /// Take @key out: its item, if it was in.
    pub fn remove<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<I>
    where I::Target: Borrow<Q>
    {
        let g = self.lock.lock();
        let s = self.locked(&g)?.find_locked(key)?;
        let old = s.load_locked();
        s.clear();
        self.nr.fetch_sub(1, Relaxed);
        old
    }

    /// Empty it, freeing its buffer after a grace period.
    pub fn clear(&self) {
        let _g = self.lock.lock();
        // SAFETY: we hold the writers' lock
        unsafe { self.table.clear() }
        self.nr.store(0, Relaxed);
    }

    /// The table's invariant, checked - every item in one of its slots, and in
    /// only one, and as many as it counts: how many, or what's broken. For
    /// tests, and debugging: it walks every slot.
    pub fn check(&self) -> Result<usize, &'static str> {
        let g = self.lock.lock();
        let Some(t) = self.locked(&g) else { return Ok(0) };
        let mut nr = 0;

        for s in t.slots {
            let Some(i) = s.load_locked() else { continue };
            match t.find_locked(t.key(&i)) {
                None => return Err("cuckoo: an item in a slot it doesn't hash to"),
                Some(f) if !core::ptr::eq(f, s) => return Err("cuckoo: an item in two slots"),
                Some(_) => nr += 1,
            }
        }
        if nr != self.nr.load(Relaxed) {
            return Err("cuckoo: an item count that isn't the items");
        }
        Ok(nr)
    }
}

// ── An entry's own registration ──────────────────────────────────────────

/// An entry's place in a map, and its key, held by the entry: dropping it
/// takes the entry out. A reference each way, owning neither - the table
/// points to the entry, the entry to the table - so the entry is pinned while
/// it's in, and leaves as it's dropped. Made by new(), the entry's Hash, Eq
/// and Borrow<K> are key()'s, which nothing can change; an entry that holds
/// its key itself is registered by register(), and this held beside it.
///
/// The table is a BackRef: it outlives its entries.
///
/// Taking the entry out is its Detach: what the entry's in is freed with it
/// detached first - and, for a Shared table's readers, a grace period on: its
/// RcuMember.
pub struct Registration<K, E, S = SipHash13Key, const W: usize = 2, M: Mode = Shared>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{
    table:   BackRef<Cuckoo<Entry<E>, S, W, M>>,
    entry:   NonNull<E>,
    key:     K,
    _detach: DetachMember,
    _rcu:    M::Member,
}

// SAFETY: shared or sent, it gives out only the key, and its drop takes the
// table's lock - a writer's, which any thread may be, a Private table's too:
// what a Private table keeps to its thread is its readers. @entry is
// compared, never followed
unsafe impl<K, E, S, const W: usize, M: Mode> Send for Registration<K, E, S, W, M>
where K: Hash + Eq + Send, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{}
unsafe impl<K, E, S, const W: usize, M: Mode> Sync for Registration<K, E, S, W, M>
where K: Hash + Eq + Sync, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{}

/// Why a Registration wasn't made: its key's in the table already - another
/// entry's, which stays - or the table couldn't grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterError {
    Exists,
    Alloc,
}

impl From<AllocError> for RegisterError {
    fn from(_: AllocError) -> Self {
        RegisterError::Alloc
    }
}

impl<K, E, S, const W: usize, M: Mode> Registration<K, E, S, W, M>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{
    /// A Shared table's readers may hold an &E past its drop: only its memory
    /// waits for them.
    const NOTHING_TO_DROP: () = assert!(!M::SHARED || !core::mem::needs_drop::<E>(),
        "a Shared cuckoo table's entries are read after they're dropped: E must have nothing to drop");

    /// Registers @e in @table under @key until this is dropped. It's made in
    /// place as a field of @e, so the insert hashes @e through it. Fails,
    /// leaving the table unchanged, if @key is in it already or it can't grow.
    ///
    /// # Safety
    /// Entry::new()'s, but for the key, which is this one's; @table outlives
    /// this; and @e is the entry this is a field of, being pin-initialized -
    /// pin-init's &this - with this initialized last, and detached before
    /// the rest of @e is dropped: @e's Detach, which its drop runs first.
    pub unsafe fn new<'w>(table: &'w Cuckoo<Entry<E>, S, W, M>, key: K, e: NonNull<E>)
        -> impl ::pin_init::PinInit<Self, RegisterError> + use<'w, K, E, S, W, M>
    {
        let () = Self::NOTHING_TO_DROP;
        let init = move |slot: *mut Self| {
            // SAFETY: @slot is ours to write, pin-init's; @table outlives
            // this, the caller's
            unsafe { slot.write(Self { table: BackRef::new(table), entry: e, key, _detach: DetachMember, _rcu: Default::default() }) };
            // SAFETY: the caller's
            let err = match table.insert_new(unsafe { Entry::new(e) }) {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(_)) => RegisterError::Exists,
                Err(_)     => RegisterError::Alloc,
            };
            // SAFETY: just written; its drop finds nothing of ours to take
            // out - an entry that's in already isn't @e
            unsafe { core::ptr::drop_in_place(slot) };
            Err(err)
        };
        // SAFETY: @slot is initialized on Ok, and not on Err; nothing moves it
        unsafe { ::pin_init::pin_init_from_closure(init) }
    }

    /// Registers @e in @table under @key until this is dropped, for an entry
    /// that holds its key itself, rather than through this: it's inserted
    /// first, and this is held beside it - an Option, say. The entry doesn't
    /// move while it's in; this can. Fails as new() does.
    ///
    /// # Safety
    /// Entry::new()'s, for @e, whose key is @key while it's in; and @table
    /// outlives this.
    pub unsafe fn register(table: &Cuckoo<Entry<E>, S, W, M>, key: K, e: NonNull<E>) -> Result<Self, RegisterError> {
        let () = Self::NOTHING_TO_DROP;
        // SAFETY: the caller's
        match table.insert_new(unsafe { Entry::new(e) }) {
            // SAFETY: the caller's
            Ok(Ok(())) => Ok(Self { table: unsafe { BackRef::new(table) }, entry: e, key, _detach: DetachMember, _rcu: Default::default() }),
            Ok(Err(_)) => Err(RegisterError::Exists),
            Err(_)     => Err(RegisterError::Alloc),
        }
    }

    pub fn key(&self) -> &K {
        &self.key
    }
}

impl<K, E, S, const W: usize, M: Mode> Detach for Registration<K, E, S, W, M>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{
    /// Out, if the slot is still this entry's: an insert() of its key may
    /// have put another in its place - or the entry's out already.
    fn detach(&self) {
        let table = &*self.table;
        let g = table.lock.lock();
        let Some(s) = table.locked(&g).and_then(|t| t.find_locked(&self.key)) else { return };
        if s.load_locked().is_some_and(|i| i.0 == self.entry) {
            s.clear();
            table.nr.fetch_sub(1, Relaxed);
        }
    }
}

impl<K, E, S, const W: usize, M: Mode> Drop for Registration<K, E, S, W, M>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: BuildHasher + From<[u64; 2]>
{
    fn drop(&mut self) {
        // not self.detach(): that's &mut Self's, which has nothing to detach
        <Self as Detach>::detach(self)
    }
}
