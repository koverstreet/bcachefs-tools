// SPDX-License-Identifier: GPL-2.0

//! A cuckoo hash table looked up and written without a lock, under RCU: a set
//! of u64s, or a map to entries that hold their own keys - in two ways or
//! three, and hashed by a BuildHasher, as std's HashMap is: SipHash-1-3, by
//! default, or hash_64()'s golden ratio (util/hash.rs) where a lookup's cost
//! matters more than keys with structure colliding.
//!
//! Keys are std's: an item - the u64, or the entry it points to - is Hash +
//! Eq, and a lookup takes anything it Borrows, as HashSet::get() does; an
//! entry's Hash and Eq are its key's. An entry the map doesn't own can hold
//! its own place in it, and its key - a Registration - so that dropping the
//! entry takes it out.

mod faults;
mod multimap;
mod table;

pub use multimap::CuckooMultimap;

#[cfg(CONFIG_BCACHEFS_TESTS)]
pub use faults::Seen;
pub use faults::Faults;

use core::borrow::Borrow;
use core::hash::{BuildHasher, Hash};
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize};
use core::sync::atomic::Ordering;

use crate::locking::{ByteLock, ExclusiveGuard, Locked};
use crate::types::CacheAligned;
use crate::util::alloc::AllocError;
use crate::util::backref::BackRef;
use crate::util::rcu::{read_lock, Detach, DetachMember, RcuArray, RcuMember, ReadGuard};
use crate::util::siphash::SipHash13Key;
use table::{Head, Mirror, Slot};

/// A slot's word, read.
#[derive(Clone, Copy)]
pub enum Contents<I> {
    Empty,
    /// Empty, held by a writer about to store into it.
    Claimed,
    /// Drained: nothing goes in it again.
    Retired,
    Item(I),
    /// An item a writer holds: on a chain being moved, or being inserted.
    Unstable(I),
}

impl<I> Contents<I> {
    fn item(self) -> Option<I> {
        match self {
            Contents::Item(i) => Some(i),
            _ => None,
        }
    }
}

/// What a table's slots hold, a word each.
///
/// XXX: not cuckoo's. Atomic, load() and store() are util/projections'
/// AtomicWord again, with orderings, and Contents for the words that aren't
/// items. And target() is Borrow, but for an entry's deref being unsafe -
/// which is the RCU types' to say: see Entry.
///
/// # Safety
/// Atomic owns nothing, so a bytewise copy is another; all zeroes is empty;
/// load() gives back what store() stored.
pub unsafe trait CuckooItem: Copy {
    type Atomic: Send + Sync;

    /// What it is, or points to: hashed and compared as its key.
    type Target: ?Sized + Hash + Eq;

    /// # Safety
    /// The item's being put in, or was read from a slot in a read-side
    /// section: an entry's valid until a grace period after it's out.
    unsafe fn target(&self) -> &Self::Target;

    /// Whether it can go in a table at all.
    fn storable(self) -> bool {
        true
    }

    fn load(a: &Self::Atomic, o: Ordering) -> Contents<Self>;
    fn store(a: &Self::Atomic, v: Contents<Self>, o: Ordering);
    fn compare_exchange(a: &Self::Atomic, current: Contents<Self>, new: Contents<Self>, success: Ordering, failure: Ordering)
        -> Result<Contents<Self>, Contents<Self>>;
}

fn u64_word(v: Contents<u64>) -> u64 {
    match v {
        Contents::Empty => 0,
        Contents::Claimed => 1,
        Contents::Retired => 2,
        Contents::Item(k) => (k + 2) << 1,
        Contents::Unstable(k) => (k + 2) << 1 | 1,
    }
}

fn u64_contents(w: u64) -> Contents<u64> {
    match w {
        0 => Contents::Empty,
        1 => Contents::Claimed,
        2 => Contents::Retired,
        w if w & 1 == 0 => Contents::Item((w >> 1) - 2),
        w => Contents::Unstable((w >> 1) - 2),
    }
}

// SAFETY: an AtomicU64, holding the key shifted past the mark bit, past the
// words for empty - zero - claimed and retired
unsafe impl CuckooItem for u64 {
    type Atomic = AtomicU64;
    type Target = u64;

    unsafe fn target(&self) -> &u64 {
        self
    }

    fn storable(self) -> bool {
        self < (1 << 63) - 2
    }

    fn load(a: &AtomicU64, o: Ordering) -> Contents<u64> {
        u64_contents(a.load(o))
    }

    fn store(a: &AtomicU64, v: Contents<u64>, o: Ordering) {
        a.store(u64_word(v), o)
    }

    fn compare_exchange(a: &AtomicU64, current: Contents<u64>, new: Contents<u64>, success: Ordering, failure: Ordering)
        -> Result<Contents<u64>, Contents<u64>>
    {
        a.compare_exchange(u64_word(current), u64_word(new), success, failure)
            .map(u64_contents).map_err(u64_contents)
    }
}

/// @ty hashed, compared and borrowed as the key @key gives - `|e| &e.key`, or
/// a method's - for a map's entry, which is its key: the derives would
/// compare all of it. `impl[...]` first for generics.
#[macro_export]
macro_rules! keyed_by {
    (impl[$($gen:tt)*] $ty:ty, |$s:ident| $key:expr => $k:ty) => {
        impl<$($gen)*> ::core::hash::Hash for $ty {
            fn hash<H: ::core::hash::Hasher>(&self, h: &mut H) {
                let $s = self;
                ::core::hash::Hash::hash($key, h)
            }
        }

        impl<$($gen)*> ::core::cmp::PartialEq for $ty {
            fn eq(&self, o: &Self) -> bool {
                let a: &$k = { let $s = self; $key };
                let $s = o;
                a == $key
            }
        }

        impl<$($gen)*> ::core::cmp::Eq for $ty {}

        impl<$($gen)*> ::core::borrow::Borrow<$k> for $ty {
            fn borrow(&self) -> &$k {
                let $s = self;
                $key
            }
        }
    };
    ($ty:ty, |$s:ident| $key:expr => $k:ty) => {
        $crate::keyed_by!(impl[] $ty, |$s| $key => $k);
    };
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
    /// long as it's in a table and a grace period after - in it, too, while
    /// an insert of it that fails is in flight, as other writers read what
    /// they meet: removed, or not put in, it's freed only after
    /// synchronize_rcu(), or by kfree_rcu() - or at once, from a Private
    /// table, which no other thread reads or writes.
    pub unsafe fn new(e: NonNull<E>) -> Self {
        const { assert!(align_of::<E>() >= 4, "cuckoo: an entry's low bits are the mark, claimed and retired") };
        Self(e)
    }

    pub fn as_ptr(self) -> NonNull<E> {
        self.0
    }

    fn word(v: Contents<Self>) -> *mut E {
        match v {
            Contents::Empty => core::ptr::null_mut(),
            Contents::Claimed => core::ptr::without_provenance_mut(1),
            Contents::Retired => core::ptr::without_provenance_mut(2),
            Contents::Item(e) => e.0.as_ptr(),
            Contents::Unstable(e) => e.0.as_ptr().map_addr(|a| a | 1),
        }
    }

    fn contents(p: *mut E) -> Contents<Self> {
        let item = |p: *mut E| Self(NonNull::new(p.map_addr(|a| a & !1)).expect("cuckoo: a mark on null"));
        match p.addr() {
            0 => Contents::Empty,
            1 => Contents::Claimed,
            2 => Contents::Retired,
            a if a & 1 != 0 => Contents::Unstable(item(p)),
            _ => Contents::Item(item(p)),
        }
    }
}

// SAFETY: an AtomicPtr: null is empty, 1 claimed and 2 retired - no entry's
// at either, as entries are 4-aligned - and the low bit the mark
unsafe impl<E: Hash + Eq + Sync> CuckooItem for Entry<E> {
    type Atomic = AtomicPtr<E>;
    type Target = E;

    unsafe fn target(&self) -> &E {
        // SAFETY: the caller's, and new()'s
        unsafe { self.0.as_ref() }
    }

    fn load(a: &AtomicPtr<E>, o: Ordering) -> Contents<Self> {
        Self::contents(a.load(o))
    }

    fn store(a: &AtomicPtr<E>, v: Contents<Self>, o: Ordering) {
        a.store(Self::word(v), o)
    }

    fn compare_exchange(a: &AtomicPtr<E>, current: Contents<Self>, new: Contents<Self>, success: Ordering, failure: Ordering)
        -> Result<Contents<Self>, Contents<Self>>
    {
        a.compare_exchange(Self::word(current), Self::word(new), success, failure)
            .map(Self::contents).map_err(Self::contents)
    }
}

/// Who uses a table: any thread, under RCU - Shared - or only its owner's -
/// Private, the table !Sync and its Registrations !Send, so no other can.
/// An entry a Shared table's readers or writers may have met stays valid a
/// grace period after it's out, so its Registration carries an RcuMember:
/// what it's in is freed a grace period on. A Private table's entries may go
/// at once.
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

/// A BuildHasher a buffer's seeds key.
pub trait CuckooHasher: BuildHasher + From<[u64; 2]> {}

impl<S: BuildHasher + From<[u64; 2]>> CuckooHasher for S {}

/// See the top of the file: of @I - u64s, or Entry<E> - hashed by @S, in @W
/// ways, two or three, read as @M says. Zeroed, it's empty, with no buffer:
/// the first insert allocates one.
///
/// repr(C): what a hit reads, which only a rehash writes - and the faults, in
/// a test - apart from what writers write on every insert.
#[repr(C)]
pub struct Cuckoo<I: CuckooItem = u64, S = SipHash13Key, const W: usize = 2, M: Mode = Shared> {
    mirror:  Mirror,
    tables:  RcuArray<Slot<I>, Head>,
    faults:  Faults,
    _writer: CacheAligned,
    /// Rehashes', and clear()'s
    lock:    Locked<ByteLock>,
    /// Items moved along claimed chains, ever: read only by a lookup that misses.
    moves:   AtomicU64,
    /// Items, for when to grow.
    nr:      AtomicUsize,
    hash:    PhantomData<fn() -> S>,
    _mode:   PhantomData<M::Marker>,
}

/// The rehash lock, held.
type Held<'a> = ExclusiveGuard<'a, ByteLock>;

// SAFETY: atomics, an RcuArray, no bytes, an unlocked ByteLock, Faults'
// integers and atomics, and PhantomDatas: all valid zeroed
unsafe impl<I: CuckooItem, S: CuckooHasher, const W: usize, M: Mode> ::pin_init::Zeroable for Cuckoo<I, S, W, M> {}

impl<I: CuckooItem, S: CuckooHasher, const W: usize, M: Mode> Default for Cuckoo<I, S, W, M> {
    fn default() -> Self {
        ::pin_init::Zeroable::zeroed()
    }
}

impl<I: CuckooItem, S: CuckooHasher, const W: usize, M: Mode> Cuckoo<I, S, W, M> {
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
        self.nr.load(Ordering::Relaxed)
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
        let mut t = Self::default();
        t.faults = faults;
        t
    }

    #[cfg(CONFIG_BCACHEFS_TESTS)]
    pub fn faults(&self) -> &Faults {
        &self.faults
    }

    #[cfg(CONFIG_BCACHEFS_TESTS)]
    pub fn moved(&self) -> u64 {
        self.moves.load(Ordering::Relaxed)
    }
}

impl<E: Hash + Eq + Sync, S: CuckooHasher, const W: usize, M: Mode> Cuckoo<Entry<E>, S, W, M> {
    /// @key's entry, for as long as @g's read-side section.
    pub fn get_entry<'g, Q: ?Sized + Hash + Eq>(&self, key: &Q, g: &'g ReadGuard) -> Option<&'g E>
    where E: Borrow<Q>
    {
        // SAFETY: in the table during @g's section, so valid until it ends
        // (Entry::new())
        self.get_rcu(key, g).map(|e| unsafe { e.0.as_ref() })
    }

    /// @e out, if it's what @key has - not another entry put in its place:
    /// whether it was.
    pub fn remove_entry<Q: ?Sized + Hash + Eq>(&self, key: &Q, e: NonNull<E>) -> bool
    where E: Borrow<Q>
    {
        self.take_if(key, |i| i.0 == e).is_some()
    }
}

impl<I: CuckooItem, S: CuckooHasher, const W: usize, M: Mode> Cuckoo<I, S, W, M> {
    /// @item, in place of what had its key, if anything: that. Fails, with
    /// the table unchanged, only if it can't allocate.
    pub fn insert(&self, item: I) -> Result<Option<I>, AllocError> {
        self.put(item, true, false)
    }

    /// @item, if nothing has its key: else what does, as the error, and @item
    /// isn't in. Fails, with the table unchanged, only if it can't allocate.
    pub fn insert_new(&self, item: I) -> Result<Result<(), I>, AllocError> {
        Ok(self.put(item, false, false)?.map_or(Ok(()), Err))
    }

    /// insert_new(), for what mustn't fail to allocate: __GFP_NOFAIL, if the
    /// table has to grow - which make_room() makes rare.
    pub fn insert_new_nofail(&self, item: I) -> Result<(), I> {
        self.put(item, false, true).expect("a nofail insert failed to allocate").map_or(Ok(()), Err)
    }

    /// Past three quarters full, a rehash now: for a caller that can block,
    /// ahead of inserts that may not - an insert that finds its table full
    /// grows it, under whatever locks it holds. Fails, the table unchanged,
    /// only if it can't allocate.
    pub fn make_room(&self) -> Result<(), AllocError> {
        self.grow_early()
    }

    /// Take @key out: its item, if it was in.
    pub fn remove<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<I>
    where I::Target: Borrow<Q>
    {
        self.take_if(key, |_| true)
    }

    /// Empty it, freeing its buffer after a grace period.
    pub fn clear(&self) {
        self.empty(&self.lock.lock())
    }

    /// The table's invariant, checked - every item in one of its slots, and in
    /// only one, and as many as it counts: how many, or what's broken. For
    /// tests, and debugging: it walks every slot.
    pub fn check(&self) -> Result<usize, &'static str> {
        self.check_invariant(&self.lock.lock())
    }
}

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
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: CuckooHasher
{
    table:   BackRef<Cuckoo<Entry<E>, S, W, M>>,
    entry:   NonNull<E>,
    key:     K,
    _detach: DetachMember,
    _rcu:    M::Member,
}

// SAFETY: sent, it gives out only the key, and its drop takes the entry out,
// a write, which any thread may do to a Shared table. A Private table's
// entries are freed at once, so its writers, as its readers, are its own
// thread: no Send for one of its. Shared, it gives out only the key. @entry
// is compared, never followed
unsafe impl<K, E, S, const W: usize> Send for Registration<K, E, S, W, Shared>
where K: Hash + Eq + Send, E: Hash + Eq + Sync + Borrow<K>, S: CuckooHasher
{}
unsafe impl<K, E, S, const W: usize, M: Mode> Sync for Registration<K, E, S, W, M>
where K: Hash + Eq + Sync, E: Hash + Eq + Sync + Borrow<K>, S: CuckooHasher
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
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: CuckooHasher
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
            let Err(err) = Self::insert(table, unsafe { Entry::new(e) }) else { return Ok(()) };
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
        Self::insert(table, unsafe { Entry::new(e) })?;
        // SAFETY: the caller's
        Ok(Self { table: unsafe { BackRef::new(table) }, entry: e, key, _detach: DetachMember, _rcu: Default::default() })
    }

    fn insert(table: &Cuckoo<Entry<E>, S, W, M>, e: Entry<E>) -> Result<(), RegisterError> {
        table.insert_new(e)?.map_err(|_| RegisterError::Exists)
    }

    pub fn key(&self) -> &K {
        &self.key
    }
}

impl<K, E, S, const W: usize, M: Mode> Detach for Registration<K, E, S, W, M>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: CuckooHasher
{
    /// Out, if the slot is still this entry's: an insert() of its key may
    /// have put another in its place - or the entry's out already.
    fn detach(&self) {
        let table = &*self.table;
        table.remove_entry(&self.key, self.entry);
    }
}

impl<K, E, S, const W: usize, M: Mode> Drop for Registration<K, E, S, W, M>
where K: Hash + Eq, E: Hash + Eq + Sync + Borrow<K>, S: CuckooHasher
{
    fn drop(&mut self) {
        // not self.detach(): that's &mut Self's, which has nothing to detach
        <Self as Detach>::detach(self)
    }
}
