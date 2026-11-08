// SPDX-License-Identifier: GPL-2.0

//! A multimap, as rhltable is: any number of values under a key. A Cuckoo of
//! groups - a group, a key's values, under a spinlock of its own. A lookup
//! finds a key's group lockless, and takes its lock in the same read-side
//! section, so nothing follows a group past its grace period.
//!
//! A group is in the table while it holds a value: an add makes it, and the
//! remove of its last value takes it out, under its lock. So an add that finds
//! its group empty, under the lock, has met it on its way out, and goes
//! again: once, as the remove took it out before letting go.

use super::{Cuckoo, CuckooHasher, Entry};
use crate::data_structures::dst_vec::DstVec;
use crate::data_structures::inline_vec::InlineVec;
use crate::errcode::BchError;
use crate::locking::{Locked, MemallocFlags, SpinLock};
use crate::new_lock;
use crate::util::alloc::{AllocError, Kmalloc};
use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::rcu::{read_lock, RcuMember};
use crate::util::siphash::SipHash13Key;
use core::hash::Hash;
use core::ptr::NonNull;
use pin_init::{pin_data, pin_init};

/// Its key, hashed and compared as the group, and its values. Readers past
/// a lookup reach only the key, the lock and, under it, the vec's inline
/// part: dropped, it frees none of them - the key's Copy, and an emptied
/// group's spill is taken out first - and its memory goes a grace period on.
#[pin_data]
struct Group<K, V, const N: usize> {
    key:  K,
    #[pin]
    vals: Locked<SpinLock, InlineVec<V, N>>,
    _rcu: RcuMember,
}

crate::keyed_by!(impl[K: Hash + Eq, V, const N: usize] Group<K, V, N>, |g| &g.key => K);

type GroupBox<K, V, const N: usize> = MaybeRcuAlloc<Group<K, V, N>, Kmalloc>;

/// @V's under @K - @N of a key's in its group itself, more on the heap -
/// hashed by @S in @W ways. Empty when it's dropped: a group in it is leaked.
pub struct CuckooMultimap<K, V, const N: usize, S = SipHash13Key, const W: usize = 2>
where K: Hash + Eq + Sync, V: Send
{
    table: Cuckoo<Entry<Group<K, V, N>>, S, W>,
}

impl<K, V, const N: usize, S: CuckooHasher, const W: usize> Default for CuckooMultimap<K, V, N, S, W>
where K: Hash + Eq + Sync, V: Send
{
    fn default() -> Self {
        Self { table: Cuckoo::default() }
    }
}

impl<K, V, const N: usize, S: CuckooHasher, const W: usize> CuckooMultimap<K, V, N, S, W>
where K: Hash + Eq + Copy + Send + Sync, V: Copy + Eq + Send + Sync
{
    /// @v under @key: one more, if it's there already. Fails only if it can't
    /// allocate: a group grows under its lock without reclaim, and if it
    /// can't, into room made outside it.
    pub fn add(&self, key: K, v: V) -> Result<(), AllocError> {
        let mut room = None;
        'add: loop {
            let want = 'group: {
                let g = read_lock();
                let Some(group) = self.table.get_entry(&key, &g) else { break 'group None };
                let mut vals = group.vals.lock();
                if vals.is_empty() {
                    continue 'add;
                }
                if let Some(r) = room.take() {
                    room = vals.grow_into(r).err();
                }
                let _noreclaim = MemallocFlags::noreclaim();
                match vals.push(v) {
                    Ok(()) => return Ok(()),
                    Err(_) => Some(vals.capacity() * 2),
                }
            };
            if let Some(cap) = want {
                room = Some(DstVec::with_capacity(cap)?);
                continue;
            }

            const { assert!(N >= 1, "cuckoo multimap: a group holds its first value itself") };
            let mut vals = InlineVec::new();
            vals.push(v)?;
            let group = GroupBox::new(pin_init!(Group {
                key,
                vals <- new_lock!(vals),
                _rcu: RcuMember::default(),
            }? BchError)).map_err(|_| AllocError)?;
            let p = group.into_raw();

            // SAFETY: a GroupBox's: valid a grace period after it's dropped
            match self.table.insert_new(unsafe { Entry::new(p) }) {
                Ok(Ok(())) => return Ok(()),
                // another add's in first; ours may have been read, by writers
                // that met it, and is freed after them
                Ok(Err(_)) => drop(unsafe { GroupBox::from_raw(p) }),
                Err(e) => {
                    drop(unsafe { GroupBox::from_raw(p) });
                    return Err(e);
                }
            }
        }
    }

    /// One @v under @key out: whether there was one.
    pub fn remove(&self, key: &K, v: &V) -> bool {
        let (spilled, p) = {
            let g = read_lock();
            let Some(group) = self.table.get_entry(key, &g) else { return false };
            let mut vals = group.vals.lock();
            let Some(i) = vals.iter().position(|x| x == v) else { return false };
            let last = vals.len() - 1;
            vals.swap(i, last);
            vals.truncate(last);
            if !vals.is_empty() {
                return true;
            }

            // An add or a with() waiting on the lock reads the vec, after we've
            // let go: an inline one, in the group's memory - not a spill's
            // heap, freed as we drop it
            let spilled = core::mem::take(&mut *vals);
            let p = NonNull::from(group);
            let was = self.table.remove_entry(key, p);
            assert!(was, "cuckoo multimap: an emptied group not in its table");
            (spilled, p)
        };
        drop(spilled);
        // SAFETY: the table's, now out of it
        drop(unsafe { GroupBox::from_raw(p) });
        true
    }

    /// @f of @key's values - none, if it has none - under its group's lock:
    /// @f mustn't sleep.
    pub fn with<R>(&self, key: &K, f: impl FnOnce(&[V]) -> R) -> R {
        let g = read_lock();
        match self.table.get_entry(key, &g) {
            Some(group) => f(&group.vals.lock()),
            None => f(&[]),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.table.len() == 0
    }
}
