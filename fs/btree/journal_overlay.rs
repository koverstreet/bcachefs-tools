// SPDX-License-Identifier: GPL-2.0

//! The journal keys: the keys recovery read from the journal, sorted, which
//! btree lookups see overlaid on the btree until journal replay has put them
//! in it - so the btree iterator, and BtreeAndJournalIter for walking a
//! node before the btree runs, read the two merged.
//!
//! Terms:
//!  - A key's index is its place in the order the keys are sorted in: btree,
//!    level - descending - and position. Equal keys sort oldest first, and
//!    sorting keeps only the newest, but for accounting keys, whose updates
//!    are deltas, each compared against the btree's version in replay.
//!  - The keys are a GapBuffer (data_structures/gap_buffer.rs): recovery's inserts come
//!    in order, so each moves little.
//!  - A key is overwritten when replay, or a btree update before replay is
//!    done, has made it obsolete: readers skip it. A run of overwritten keys
//!    is an overwrite range, [start, end) by index, which every key in the
//!    run names by its overwritten_range - 0 for none - so a reader skips the
//!    run in one step. Overwriting a key extends its neighbour's range, joins
//!    two, or starts one with a neighbour.
//!
//! Who changes what:
//!  - The keys themselves, and pre_sort, change only where nothing else reads
//!    them: before the filesystem can go rw - sorting, inserts, accounting
//!    read's merging - which a NotRw shows, their LockedBy's; the last ref's
//!    put, and what keys_mut() is for.
//!  - Overwriting happens later too, from any thread, under overwrite_lock -
//!    which journal replay's commits take, in C - while the keys are read
//!    without it. So a key's overwritten flag and range are words the lock
//!    publishes, as the ranges are an RcuArray it publishes: a reader loads a
//!    key's range index, which is stored after the range, then reads the range.
//!  - An insert bumps seq, and an iterator that finds it changed searches
//!    again for where it was - from where it started, or the
//!    BtreeAndJournalIter's pos if that's later: what's before either it
//!    never returns.

#![allow(non_camel_case_types)]

use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::accounting;
use crate::btree::bkey::{BkeyI, BkeySC, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::node::{iter::KeyIter, BtreeNode};
use crate::btree::path::LockedNode;
use crate::c;
use crate::errcode::BchError;
use crate::fs::{btree_id_is_alloc, Fs};
use crate::init::recovery::NotRw;
use crate::locking::{ExclusiveGuard, Guard as _, Locked, LockedBy, Mut, Ref};
use crate::util::projections::{Volatile, VolatilePublish};
use crate::util::alloc::KVVec;
use crate::data_structures::gap_buffer::GapBuffer;
use crate::os::kernel::cond_resched;
use crate::util::log::{LogLevel, LogMsg};
use crate::util::place::Placed;
use crate::util::rcu::{self, ReadGuard, RcuArray, RcuElement};
use crate::util::refcount::{AtomicRef, RefGuard, TryRefcount, WaitableRef};
use crate::{bch_err, field, prt, new_lock, published};
use core::cmp::Ordering;
use core::mem::size_of;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{bitfield, MaybeZeroable, Init, BitfieldMembers};
use c::Errcode as e;
use c::FsFlag as f;
use crate::journal::JournalEntries;
use crate::types::{c_default};
use crate::util::vstructs::Head;
use core::sync::atomic::{AtomicU64, AtomicUsize};
use nestify::nest;
use pin_init::{pin_data, pinned_drop};
use crate::format::{Bpos, Btree, BkeyType};

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct journal_ptr {
    pub csum_good: bool,
    pub csum: c::bch_csum,
    pub dev: u8,
    pub bucket: u32,
    pub bucket_offset: u32,
    pub sector: u64,
}

/*
 * Only used for holding the journal entries we read in btree_journal_read()
 * during cache_registration
 */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct journal_replay {
    pub ptrs: KVVec<c::journal_ptr>,

    pub csum_good: bool,
    pub ignore_blacklisted: bool,
    pub ignore_not_dirty: bool,
    /* must be last: */
    pub j: Head<c::jset>,
}

nest! {
    #[derive(MaybeZeroable)]*
    #[derive(BitfieldMembers)]
    #[repr(C)]
    pub struct journal_key {
        #>[repr(C)]
        #>[derive(Clone, Copy)]
        pub src: pub union journal_key_src {
            #>[repr(C)]
            #>[derive(Clone, Copy)]
            pub journal: pub struct journal_key_journal_pos {
                pub journal_seq_offset: u32,
                pub journal_offset: u32,
            },
            pub allocated_k: *mut c::bkey_i,
        },
        #[members]
        #>[derive(MaybeZeroable)]-
        #>[bitfield(u16)]
        pub id: pub struct journal_key_id {
            #[bits(8)] pub btree_id: Btree,
            level:       u8,
        },
        #[members]
        #>[derive(MaybeZeroable)]-
        #>[bitfield(u8)]
        pub flags: pub struct journal_key_flags {
            allocated:   bool,
            rewind:      bool,
            __pad:       u6,
        },
        /*
         * Set, and overwritten_range, under overwrite_lock, while the keys
         * are read without it: in a byte of their own
         */
        pub overwritten: VolatilePublish<u8, OverwriteRanges>,
        pub overwritten_range: VolatilePublish<u32, OverwriteRanges>,
    }
}
c_default!(journal_key);

/// A run of overwritten keys, [start, end) by index, that a reader skips in
/// one step: btree/journal_overlay.rs.
#[repr(C)]
#[derive(Default)]
pub struct OverwriteRange {
    pub start: AtomicUsize,
    pub end:   AtomicUsize,
}

// SAFETY: atomics, valid zeroed, owning nothing
unsafe impl RcuElement for OverwriteRange {}

/* overwrite_lock's: how many of the ranges are in use, the first a sentinel */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct OverwriteRanges {
    pub overwrites_nr: usize,
}

published! {
    /// The ranges, read without overwrite_lock under RCU: a key's
    /// overwritten_range indexes them.
    #[derive(MaybeZeroable)]
    pub struct OverwriteRangesLive {
        pub ranges: RcuArray<OverwriteRange>,
    }
}

/// The value of the filesystem's AtomicRef, Fs.journal_keys: recovery's ref
/// is its owner's, stopped once replay's done with the keys, and the last
/// put drops them.
#[pin_data(PinnedDrop)]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct journal_keys {
    /*
     * The keys, a gap buffer: data_structures/gap_buffer.rs, whose fields C reads as
     * its own. Written only where nothing else reads them: before the
     * filesystem can go rw - and by the last ref, and keys_mut()'s
     */
    pub keys: LockedBy<GapBuffer<c::journal_key>, (), NotRw<'static>>,
    /* Bumped by each insert: an iterator that saw another searches again */
    pub seq: AtomicU64,

    /*
     * Keys inserted before journal_keys_sort() (e.g. from
     * bch2_dev_usage_init during offline device add). These survive
     * the reset in journal_keys_sort and get merged into the main
     * array after sorting. Written as the keys are.
     */
    pub pre_sort: LockedBy<KVVec<c::journal_key>, (), NotRw<'static>>,

    /*
     * What the journal read kept, by seq from journal_entries_base_seq: the
     * keys'. Written only before the filesystem can go rw.
     */
    pub(crate) entries: LockedBy<JournalEntries, (), NotRw<'static>>,

    #[pin]
    #[init(new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&keys->overwrite_lock"))]
    pub overwrites: Locked<c::mutex, OverwriteRanges, OverwriteRangesLive>,
}

pub(crate) type OverwriteGuard<'a> = ExclusiveGuard<'a, c::mutex, c::OverwriteRanges, c::OverwriteRangesLive>;

pub type JournalKeysRef = RefGuard<Placed<AtomicRef<c::journal_keys>, crate::at!(Fs.journal_keys)>>;

impl Fs {
    /// A ref on the journal keys: None once recovery's done with them.
    pub fn journal_keys_get(&self) -> Option<JournalKeysRef> {
        self.journal_keys.tryget()
    }

    /// The journal keys, for a C transaction's ref: its
    /// journal_replay_not_finished.
    ///
    /// # Safety
    /// A ref is held for 'a.
    pub(crate) unsafe fn journal_keys_held(&self) -> &c::journal_keys {
        unsafe { self.journal_keys.value_held() }
    }
}

impl c::journal_key {
    pub(crate) fn is_overwritten(&self) -> bool {
        self.overwritten.get() != 0
    }

    /// Its overwrite range's index, and the range itself published.
    fn range(&self) -> u32 {
        self.overwritten_range.get_acquire()
    }

    /// For @k, kmalloc()'d, a key of its own.
    fn new_allocated(btree: Btree, level: u32, k: *mut c::bkey_i) -> Self {
        let mut n = Self::default();
        n.id = c::journal_key_id::new().with_btree_id(btree).with_level(level as u8);
        n.flags = c::journal_key_flags::new().with_allocated(true);
        n.src.allocated_k = k;
        n
    }

    /// Where in the journal it was read from: None for a key inserted since.
    pub(crate) fn journal_pos(&self) -> Option<c::journal_key_journal_pos> {
        (!self.allocated()).then(|| unsafe { self.src.journal })
    }
}

impl Drop for c::journal_key {
    fn drop(&mut self) {
        if self.allocated() {
            // SAFETY: new_allocated()'s, kmalloc()'d: its own
            unsafe { c::rust_kfree(self.src.allocated_k.cast()) };
        }
    }
}

/// Of a key at @level in @btree and key @r: which btree and level sorts
/// first.
#[inline]
fn btree_cmp(btree: Btree, level: u32, r: &c::journal_key) -> Ordering {
    u32::from(r.level()).cmp(&level).then(btree.0.cmp(&r.btree_id().0))
}

impl Fs {
    /// The seq of the journal entry @k was read from: None for a key inserted
    /// since.
    pub(crate) fn journal_key_seq(&self, k: &c::journal_key) -> Option<u64> {
        k.journal_pos().map(|pos| self.journal_entries_base_seq.get() + u64::from(pos.journal_seq_offset))
    }
}

impl c::journal_keys {
    /// Key @k: its own, or in the journal entry it was read from -
    /// journal_key_k().
    #[inline]
    pub(crate) fn key_k(&self, k: &c::journal_key) -> *mut c::bkey_i {
        // SAFETY: a key read from the journal is in an entry that lives as
        // long as the keys, at its offset
        unsafe {
            if k.allocated() {
                return k.src.allocated_k;
            }
            let pos = k.src.journal;
            let i = self.entries().at_offset(pos.journal_seq_offset).expect("a journal key's entry");
            (&raw const (**i).j._data).cast::<u64>().add(pos.journal_offset as usize).cast_mut().cast()
        }
    }

    #[inline]
    pub(crate) fn key<'a>(&'a self, k: &'a c::journal_key) -> &'a BkeyI {
        unsafe { &*BkeyI::from_raw(self.key_k(k)) }
    }

    #[inline]
    pub(crate) fn key_pos(&self, k: &c::journal_key) -> Bpos {
        self.key(k).k.p
    }

    #[inline]
    pub(crate) fn key_cmp_pos(&self, btree: Btree, level: u32, pos: Bpos, r: &c::journal_key) -> Ordering {
        btree_cmp(btree, level, r).then_with(|| pos.cmp(&self.key_pos(r)))
    }

    pub(crate) fn key_cmp(&self, l: &c::journal_key, r: &c::journal_key) -> Ordering {
        self.key_cmp_pos(l.btree_id(), l.level().into(), self.key_pos(l), r)
    }

    /// The index of the first key at or after @pos in @btree at @level:
    /// __bch2_journal_key_search().
    pub(crate) fn search(&self, btree: Btree, level: u32, pos: Bpos) -> usize {
        let keys = self.keys();
        let (mut l, mut r) = (0, keys.len());

        while l < r {
            let m = l + (r - l) / 2;
            if self.key_cmp_pos(btree, level, pos, &keys[m]).is_gt() {
                l = m + 1;
            } else {
                r = m;
            }
        }

        assert!(l == keys.len() || self.key_cmp_pos(btree, level, pos, &keys[l]).is_le());
        assert!(l == 0 || self.key_cmp_pos(btree, level, pos, &keys[l - 1]).is_gt());
        l
    }

    /// The entries the journal read kept, and their jsets, but those replay
    /// ignores.
    pub fn replays(&self) -> impl Iterator<Item = (&c::journal_replay, &c::jset)> {
        self.all_replays().filter(|(i, _)| !i.ignored())
    }

    /// Every entry the journal read kept, and its jset, ignored or not.
    pub fn all_replays(&self) -> impl Iterator<Item = (&c::journal_replay, &c::jset)> {
        self.entries().iter().map(|i| (&**i, i.jset()))
    }

    /// all_replays(), newest first.
    pub fn all_replays_rev(&self) -> impl Iterator<Item = (&c::journal_replay, &c::jset)> {
        self.entries().iter_rev().map(|i| (&**i, i.jset()))
    }

    pub fn entries(&self) -> &JournalEntries {
        unsafe { &*self.entries.data_ptr() }
    }

    pub fn keys(&self) -> &GapBuffer<c::journal_key> {
        // written only with nothing reading
        unsafe { &*self.keys.data_ptr() }
    }

    /// # Safety
    /// Nothing else reads or writes the keys while it's held: the last ref,
    /// or a writer that can't have a NotRw.
    pub(crate) unsafe fn keys_mut(&self) -> &mut GapBuffer<c::journal_key> {
        unsafe { self.keys.protected_mut() }
    }

    fn seq(&self) -> u64 {
        self.seq.load(Relaxed)
    }

    /// Overwrite range @r, a key's that's overwritten.
    fn overwrite_range<'a>(&'a self, r: u32, rcu: &'a ReadGuard) -> &'a c::OverwriteRange {
        &self.overwrites.ranges.read(rcu)[r as usize]
    }

    /// Key @idx overwritten: and in a neighbour's range, joining the two if
    /// both have one, or a new range with the neighbours that are overwritten.
    /// Without a new range if there's no memory for one - readers step over
    /// it alone. __bch2_journal_key_overwritten().
    fn overwrite(&self, g: &mut OverwriteGuard<'_>, idx: usize) {
        let keys = self.keys();
        let k = &keys[idx];
        k.overwritten.set(1, g);

        let prev = idx.checked_sub(1).and_then(|i| keys.get(i)).filter(|k| k.is_overwritten());
        let next = keys.get(idx + 1).filter(|k| k.is_overwritten());
        let prev_r = prev.map(|k| k.range()).filter(|&r| r != 0);
        let next_r = next.map(|k| k.range()).filter(|&r| r != 0);

        if prev_r.is_none() && next_r.is_none() {
            if prev.is_none() && next.is_none() {
                return;
            }

            /* 0 is a sentinel */
            let r = g.overwrites_nr.max(1);
            if g.published().ranges.grow_to(r + 1).is_err() {
                return;
            }
            g.overwrites_nr = r + 1;

            let rcu = rcu::read_lock();
            let range = self.overwrite_range(r as u32, &rcu);
            range.start.store(idx - prev.is_some() as usize, Relaxed);
            range.end.store(idx + 1 + next.is_some() as usize, Relaxed);

            for k in [Some(k), prev, next].into_iter().flatten() {
                k.overwritten_range.set_release(r as u32, g);
            }
            return;
        }

        let rcu = rcu::read_lock();
        let prev_range = prev_r.map(|r| (r, self.overwrite_range(r, &rcu)));
        let next_range = next_r.map(|r| (r, self.overwrite_range(r, &rcu)));

        if let Some((_, p)) = prev_range {
            assert_eq!(p.end.load(Relaxed), idx, "overwrite range before key {idx}");
        }
        if let Some((_, n)) = next_range {
            assert_eq!(n.start.load(Relaxed), idx + 1, "overwrite range after key {idx}");
        }

        match (prev_range, next_range) {
            (Some((pr, p)), Some((nr, n))) => {
                p.end.store(n.end.load(Relaxed), Relaxed);
                k.overwritten_range.set_release(pr, g);

                for i in n.start.load(Relaxed)..n.end.load(Relaxed) {
                    let ik = &keys[i];
                    assert_eq!(ik.range(), nr, "key {i} in overwrite range {nr}");
                    ik.overwritten_range.set_release(pr, g);
                }
            }
            (Some((pr, p)), None) => {
                p.end.fetch_add(1, Relaxed);
                k.overwritten_range.set_release(pr, g);
                if let Some(next) = next {
                    p.end.fetch_add(1, Relaxed);
                    next.overwritten_range.set_release(pr, g);
                }
            }
            (None, Some((nr, n))) => {
                n.start.fetch_sub(1, Relaxed);
                k.overwritten_range.set_release(nr, g);
                if let Some(prev) = prev {
                    n.start.fetch_sub(1, Relaxed);
                    prev.overwritten_range.set_release(nr, g);
                }
            }
            (None, None) => unreachable!(),
        }
    }

    pub(crate) fn mark_overwritten(&self, idx: usize) {
        let mut g = self.overwrites.lock();
        if !self.keys()[idx].is_overwritten() {
            self.overwrite(&mut g, idx);
        }
    }

    /// The keys freed, what they own and the buffer, and the overwrite
    /// ranges: bch2_journal_keys_reset().
    fn reset(&self, keys: &mut GapBuffer<c::journal_key>) {
        keys.clear();

        let mut g = self.overwrites.lock();
        g.overwrites_nr = 0;
        g.published().ranges.clear();
    }

    /// The keys marked overwritten dropped, the rest kept in order.
    pub(crate) fn drop_overwritten(&self, not_rw: &mut NotRw<'_>) {
        let keys = not_rw.get_mut_of(&self.keys);
        let s = keys.make_contiguous();
        let mut nr = 0;
        for i in 0..s.len() {
            if !s[i].is_overwritten() {
                s.swap(nr, i);
                nr += 1;
            }
        }
        keys.truncate(nr);
    }

    /// Key @k, to edit in place: its own, or in the journal entry it was read
    /// from - had alone, not rw.
    pub(crate) fn key_mut<'a>(&'a self, _not_rw: &'a mut NotRw<'_>, k: &c::journal_key) -> &'a mut BkeyI {
        unsafe { &mut *BkeyI::from_raw_mut(self.key_k(k)) }
    }
}

// ── Lookups, for the btree iterator ──────────────────────────────────────

/// The first key from @pos to @end, not overwritten: searched for, or from
/// *@idx if it's close, and *@idx left at it for the next.
pub(crate) fn peek_max<'a>(jk: &'a c::journal_keys, btree: Btree, level: u32, pos: Bpos, end: Bpos,
                           idx: &mut usize) -> Option<&'a BkeyI>
{
    let keys = jk.keys();
    let mut iters = 0;

    assert!(*idx <= keys.len(), "journal key {} of {}", *idx, keys.len());
    'search: loop {
        if *idx == 0 {
            *idx = jk.search(btree, level, pos);
        }

        while *idx > 0 && jk.key_cmp_pos(btree, level, end, &keys[*idx - 1]).is_le() {
            *idx -= 1;
            iters += 1;
            if iters == 10 {
                *idx = 0;
                continue 'search;
            }
        }

        let rcu = rcu::read_lock();
        while let Some(k) = keys.get(*idx) {
            if jk.key_cmp_pos(btree, level, end, k).is_lt() {
                break;
            }

            if k.is_overwritten() {
                *idx = match k.range() {
                    0 => *idx + 1,
                    r => jk.overwrite_range(r, &rcu).end.load(Relaxed),
                };
                continue;
            }

            if jk.key_cmp_pos(btree, level, pos, k).is_le() {
                return Some(jk.key(k));
            }

            *idx += 1;
            iters += 1;
            if iters == 10 {
                *idx = 0;
                continue 'search;
            }
        }
        return None;
    }
}

pub(crate) fn peek_prev_min<'a>(jk: &'a c::journal_keys, btree: Btree, level: u32, pos: Bpos, end: Bpos,
                                idx: &mut usize) -> Option<&'a BkeyI>
{
    let keys = jk.keys();
    let mut iters = 0;

    assert!(*idx <= keys.len(), "journal key {} of {}", *idx, keys.len());
    if keys.is_empty() {
        return None;
    }
    'search: loop {
        if *idx == 0 {
            *idx = jk.search(btree, level, pos);
        }

        while *idx < keys.len() && jk.key_cmp_pos(btree, level, end, &keys[*idx]).is_ge() {
            *idx += 1;
            iters += 1;
            if iters == 10 {
                *idx = 0;
                continue 'search;
            }
        }

        if *idx == keys.len() {
            *idx -= 1;
        }

        let rcu = rcu::read_lock();
        loop {
            let k = &keys[*idx];
            if jk.key_cmp_pos(btree, level, end, k).is_gt() {
                break;
            }

            if k.is_overwritten() {
                let r = k.range();
                if r != 0 {
                    *idx = jk.overwrite_range(r, &rcu).start.load(Relaxed);
                }
                if *idx == 0 {
                    break;
                }
                *idx -= 1;
                continue;
            }

            if jk.key_cmp_pos(btree, level, pos, k).is_ge() {
                return Some(jk.key(k));
            }

            if *idx == 0 {
                break;
            }
            *idx -= 1;
            iters += 1;
            if iters == 10 {
                *idx = 0;
                continue 'search;
            }
        }
        return None;
    }
}

pub(crate) fn peek_slot(jk: &c::journal_keys, btree: Btree, level: u32, pos: Bpos) -> Option<&BkeyI> {
    peek_max(jk, btree, level, pos, pos, &mut 0)
}

/// Whether the journal deletes @pos, before replay's done.
pub(crate) fn key_deleted_in_journal(t: &TransAttempt<'_, '_>, btree: Btree, level: u32, pos: Bpos) -> bool {
    let Some(jk) = t.journal_keys() else { return false };
    let Some(k) = jk.keys().get(jk.search(btree, level, pos)) else { return false };
    if k.btree_id() != btree || u32::from(k.level()) != level {
        return false;
    }

    let k = jk.key(k);
    k.k.p == pos && k.k.key_type() == BkeyType::deleted
}

/// The key at @pos overwritten, if the journal has one that isn't - or with
/// @check, whether it has one that is.
pub(crate) fn check_or_overwrite(jk: &c::journal_keys, g: &mut OverwriteGuard<'_>, btree: Btree, level: u32,
                                 pos: Bpos, check: bool) -> bool {
    let idx = jk.search(btree, level, pos);

    let Some(k) = jk.keys().get(idx) else { return false };
    if k.btree_id() != btree || u32::from(k.level()) != level || jk.key_pos(k) != pos {
        return false;
    }

    if k.is_overwritten() {
        return check;
    }

    if !check {
        jk.overwrite(g, idx);
    }
    false
}

/// The journal's key at @pos overwritten, if it has one that isn't: the btree
/// has its update now, before replay's done.
pub(crate) fn journal_key_overwrite(jk: &c::journal_keys, btree: Btree, level: u32, pos: Bpos) {
    check_or_overwrite(jk, &mut jk.overwrites.lock(), btree, level, pos, false);
}

// ── Inserts, by recovery before the btree's read-write ───────────────────

/// @k inserted, as @btree and @level's key, replacing an equal key - adding
/// to it, for accounting - or before it. Taking @k, whose kfree() is the
/// keys' from here on - on error too. Before the btree runs, into pre_sort,
/// which sorting merges in.
///
/// # Safety
/// @k is a key, kmalloc()'d, the caller's to give.
unsafe fn insert_take(not_rw: &mut NotRw<'_>, btree: Btree, level: u32, k: *mut c::bkey_i) -> Result<(), BchError> {
    let fs = not_rw.fs;
    let jk = fs.journal_keys_get().expect("journal keys inserted after recovery was done with them");
    let jk = jk.value();
    let n = c::journal_key::new_allocated(btree, level, k);

    if !fs.flags.get(f::btree_running) {
        return not_rw.get_mut_of(&jk.pre_sort).push(n)
            .map_err(|_| fs.err(e::ENOMEM_journal_key_insert));
    }

    // SAFETY: the caller's
    let k = unsafe { &mut *BkeyI::from_raw_mut(k) };
    let idx = jk.search(btree, level, k.k.p);

    let keys = not_rw.get_mut_of(&jk.keys);

    if let Some(o) = keys.get_mut(idx).filter(|o| jk.key_cmp(&n, o).is_eq()) {
        let ok = jk.key(o);
        let accounting = k.k.key_type() == BkeyType::accounting && ok.k.key_type() == BkeyType::accounting;

        if !(accounting && !o.allocated()) {
            if accounting {
                accounting::accumulate(k, BkeySC::from(ok));
            }
            *o = n;
            return Ok(());
        }
    }

    if keys.reserve().is_err() {
        bch_err!(fs, "bch2_journal_key_insert_take: error allocating new key array (size {})",
                 keys.size.max(8) * 2);
        return fs.throw(e::ENOMEM_journal_key_insert);
    }
    keys.insert(idx, n);
    jk.seq.fetch_add(1, Relaxed);
    Ok(())
}

/// insert_take() of a copy of @k.
pub(crate) fn insert(not_rw: &mut NotRw<'_>, btree: Btree, level: u32, k: &BkeyI) -> Result<(), BchError> {
    let bytes = k.k.bytes();
    let n = crate::util::alloc::kmalloc(bytes).map_err(|_| not_rw.fs.err(e::ENOMEM_journal_key_insert))?;
    // SAFETY: @bytes of key, into as many of ours; then ours to hand over
    unsafe {
        ptr::copy_nonoverlapping(k.as_ffi().cast::<u8>(), n.as_ptr(), bytes);
        insert_take(not_rw, btree, level, n.as_ptr().cast())
    }
}

/// A deleted key at @pos: what's there in the btree, gone.
pub(crate) fn delete(not_rw: &mut NotRw<'_>, btree: Btree, level: u32, pos: Bpos) -> Result<(), BchError> {
    insert(not_rw, btree, level, BkeyI::<c::Deleted>::at(pos).as_ref())
}

// ── Iterating: a btree node's keys, with the journal's overlaid ──────────

impl c::journal_iter {
    fn new(jk: &c::journal_keys, btree: Btree, level: u32, pos: Bpos) -> Self {
        Self {
            btree_id: btree,
            level,
            idx: jk.search(btree, level, pos),
            seq: jk.seq(),
            pos,
        }
    }

    fn verify(&self, jk: &c::journal_keys) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            if let Some(k) = jk.keys().get(self.idx) {
                assert!(btree_cmp(self.btree_id, self.level, k).is_le(), "journal iter before its btree");
            }
        }
    }

    fn advance(&mut self, jk: &c::journal_keys) {
        if self.idx < jk.keys().len() {
            self.idx += 1;
        }
    }

    /// The next key not overwritten, if it's in the iterator's btree and
    /// level; after an insert, the first from @pos or where the iterator
    /// started, whichever's later.
    fn peek(&mut self, jk: &c::journal_keys, pos: Bpos) -> *const c::bkey_i {
        let seq = jk.seq();
        if self.seq != seq {
            self.idx = jk.search(self.btree_id, self.level, pos.max(self.pos));
            self.seq = seq;
        }
        self.verify(jk);

        let keys = jk.keys();
        let rcu = rcu::read_lock();
        while let Some(k) = keys.get(self.idx) {
            match btree_cmp(self.btree_id, self.level, k) {
                Ordering::Less    => break,
                Ordering::Greater => panic!("journal iter past its btree"),
                Ordering::Equal   => {}
            }

            if !k.is_overwritten() {
                return jk.key_k(k);
            }

            self.idx = match k.range() {
                0 => self.idx + 1,
                r => jk.overwrite_range(r, &rcu).end.load(Relaxed),
            };
        }
        ptr::null()
    }
}

/*
 * Iterate over keys in the btree, with keys from the journal overlaid on top:
 */
#[derive(Clone, Copy)]
pub(crate) struct BtreeAndJournalIter<'a, 't> {
    trans: &'a BtreeTrans<'t>,
    jk: Option<&'a c::journal_keys>,
    b: LockedNode<'a, BtreeNode>,
    node_iter: KeyIter,
    unpacked: c::bkey,
    peeked: c::bkey_s_c,

    journal: c::journal_iter,
    pos: Bpos,
    at_end: bool,
    pub(crate) prefetch: bool,
    pub(crate) fail_if_too_many_whiteouts: bool,
}

impl<'a, 't> BtreeAndJournalIter<'a, 't> {
    /// Over node @b's keys, from @node_iter, and the journal's from @pos.
    pub(crate) fn new(t: &TransAttempt<'a, 't>, b: LockedNode<'a, BtreeNode>, node_iter: KeyIter,
                      pos: Bpos) -> Self {
        let jk = t.journal_keys();
        let mut iter = Self {
            trans: t.trans(),
            jk,
            b,
            node_iter,
            unpacked: c::bkey::new(),
            peeked: c::bkey_s_c { k: ptr::null_mut(), v: ptr::null_mut() },
            journal: c::journal_iter::default(),
            pos: b.contents().data().min_key,
            at_end: false,
            prefetch: false,
            fail_if_too_many_whiteouts: false,
        };

        if let Some(jk) = jk {
            iter.journal = c::journal_iter::new(jk, Btree(b.c.btree_id.into()), b.c.level.into(), pos);
        }
        iter
    }

    pub(crate) fn advance(&mut self) {
        if self.pos == SPOS_MAX {
            self.at_end = true;
        } else {
            self.pos = self.pos.successor();
        }
    }

    /// The next few keys' nodes read ahead, from a copy of the iterator.
    fn prefetch(&self) {
        let fs = self.trans.fs();
        let mut iter = *self;
        let level = iter.journal.level;
        let nr = if fs.flags.get(f::started) { if level > 1 { 0 } else { 2 } } else if level > 1 { 1 } else { 16 };

        iter.prefetch = false;
        iter.fail_if_too_many_whiteouts = true;

        let mut tmp = BkeyBuf::new();
        for _ in 0..nr {
            iter.advance();
            let Some(k) = iter.peek() else { break };

            tmp.reassemble(k);
            let _ = self.trans.btree_node_prefetch(&mut self.trans.paths(), None, tmp.as_ref(),
                                                   iter.journal.btree_id, level as usize - 1);
        }
    }

    /// The next key at or after pos, the btree node's or the journal's,
    /// whichever's first - the journal's if both - skipping whiteouts. Good
    /// until the iterator moves.
    pub(crate) fn peek(&mut self) -> Option<BkeySC<'_>> {
        self.peeked = self.peek_raw();
        (!self.peeked.k.is_null()).then(|| unsafe { BkeySC::from_c(&self.peeked) })
    }

    fn peek_raw(&mut self) -> c::bkey_s_c {
        let null = c::bkey_s_c { k: ptr::null_mut(), v: ptr::null_mut() };
        let mut iters = 0;

        if self.prefetch && self.journal.level != 0 {
            self.prefetch();
        }
        loop {
            if self.at_end {
                return null;
            }

            iters += 1;
            if iters > 20 && self.fail_if_too_many_whiteouts {
                return null;
            }

            let btree_k = loop {
                let keys = self.b.keys(self.b.contents());
                let k = self.node_iter.peek_unpack(&keys, &mut self.unpacked).map_or(null, Into::into);
                // SAFETY: a key of the node's
                if k.k.is_null() || unsafe { (*k.k).p } >= self.pos {
                    break k;
                }
                self.node_iter.advance(&keys);
            };

            let mut journal_k = ptr::null();
            if let Some(jk) = self.jk {
                journal_k = loop {
                    let k = self.journal.peek(jk, self.pos);
                    // SAFETY: a journal key
                    if k.is_null() || unsafe { (*k).k.p } >= self.pos {
                        break k;
                    }
                    self.journal.advance(jk);
                };
            }

            // SAFETY: keys, as they're returned
            let mut ret = unsafe {
                if !journal_k.is_null() && (btree_k.k.is_null() || (*journal_k).k.p <= (*btree_k.k).p) {
                    BkeySC::from(&*BkeyI::from_raw(journal_k)).into()
                } else {
                    btree_k
                }
            };

            // SAFETY: as ret's
            if !ret.k.is_null() && unsafe { (*ret.k).p } > self.b.contents().data().max_key {
                ret = null;
            }

            if ret.k.is_null() {
                self.pos = SPOS_MAX;
                self.at_end = true;
                return ret;
            }

            // SAFETY: as ret's
            let k = unsafe { &*ret.k };
            self.pos = k.p;
            if k.key_type() != BkeyType::deleted {
                return ret;
            }
            self.advance();
        }
    }
}

// ── Sorting, and the keys' lifetime ───────────────────────────────────────

/// Journal keys @l and @r in sort order: of equal keys, the oldest first - of
/// two a rewind undoes, the newest - and what was read before what was
/// inserted.
fn sort_cmp(jk: &c::journal_keys, l: &c::journal_key, r: &c::journal_key) -> Ordering {
    jk.key_cmp(l, r).then_with(|| {
        if l.allocated() || r.allocated() {
            return l.allocated().cmp(&r.allocated());
        }

        // SAFETY: neither is allocated: both were read
        let (lp, rp) = unsafe { (l.src.journal, r.src.journal) };
        let o = (lp.journal_seq_offset, lp.journal_offset).cmp(&(rp.journal_seq_offset, rp.journal_offset));
        if l.flags.rewind() && r.flags.rewind() { o.reverse() } else { o }
    })
}

/// The keys sorted, and of equal keys the newest kept - but accounting keys,
/// which replay compares each against the btree's version.
fn sort_dedup(jk: &c::journal_keys, keys: &mut GapBuffer<c::journal_key>) {
    let s = keys.make_contiguous();
    let mut cmps = 0u32;
    s.sort_unstable_by(|l, r| {
        cmps = cmps.wrapping_add(1);
        if cmps % 1024 == 0 {
            cond_resched();
        }
        sort_cmp(jk, l, r)
    });

    cond_resched();

    let mut dst = 0;
    for src in 0..s.len() {
        let accounting = jk.key(&s[src]).k.key_type() == BkeyType::accounting;
        if !accounting && src + 1 < s.len() && jk.key_cmp(&s[src], &s[src + 1]).is_eq() {
            continue;
        }
        s.swap(dst, src);
        dst += 1;
    }
    keys.truncate(dst);
}

fn journal_seq_is_rewound(rewind_ranges: &[c::journal_rewind_range], seq: u64) -> bool {
    rewind_ranges.iter().any(|r| seq > r.to && seq <= r.from)
}

/// The keys, from the journal entries read - again, if they were already,
/// as for a rewind recovery decides on after a scrub of the recent entries.
pub(crate) fn keys_sort(fs: &Fs) -> Result<(), BchError> {
    let jk = fs.journal_keys_get().expect("journal keys sorted after recovery was done with them");
    let jk = jk.value();
    let mut not_rw = fs.not_rw();
    let (keys, pre_sort, rewind_ranges) = not_rw.split::<(Mut<field!(journal_keys.*.keys)>,
                                                          Mut<field!(journal_keys.*.pre_sort)>,
                                                          Ref<field!(journal.rewind_ranges)>)>();
    let (mut nr_read, mut nr_extra_sorts) = (0usize, 0usize);
    let start_seq = match fs.journal_scrub_seq.get() { 0 => fs.journal_replay_seq_start.get(), s => s };

    jk.reset(keys);

    for (i, j) in jk.replays() {
        cond_resched();

        let seq = j.seq.get();
        let seq_rewound = journal_seq_is_rewound(rewind_ranges, seq);

        if seq_rewound && !j.has_overwrites() {
            bch_err!(fs, "cannot rewind journal seq {seq}: no overwrite entries (journal_transaction_names was off)");
            return fs.throw(e::journal_rewind_no_overwrites);
        }

        for entry in j.entries() {
            let rewind = seq_rewound && entry.level == 0 && !btree_id_is_alloc(entry.btree_id.into());
            let want = if rewind {
                c::JsetEntryType::overwrite
            } else {
                c::JsetEntryType::btree_keys
            };
            if u32::from(entry.type_) != want.0 {
                continue;
            }

            /*
             * Keys from before the replay start are only for the journal
             * scrub - recovery re-sorts without them once it's done, before
             * anything replays them:
             */
            if !rewind && seq < start_seq {
                continue;
            }

            for k in entry.entries() {
                let mut n = c::journal_key::default();
                n.id = c::journal_key_id::new()
                    .with_btree_id(Btree(entry.btree_id.into()))
                    .with_level(entry.level);
                n.flags = c::journal_key_flags::new().with_rewind(rewind);
                n.src.journal = c::journal_key_journal_pos {
                    journal_seq_offset: (seq - fs.journal_entries_base_seq.get()) as u32,
                    journal_offset: ((k.as_bytes().as_ptr() as usize - (&raw const i.j._data) as usize)
                                     / size_of::<u64>()) as u32,
                };

                if keys.reserve().is_err() {
                    sort_dedup(jk, keys);
                    nr_extra_sorts += 1;

                    if keys.len() * 8 > keys.size * 7 {
                        bch_err!(fs, "Too many journal keys for slowpath; have {} compacted, buf size {}, \
                                      processed {nr_read} keys at seq {seq}", keys.len(), keys.size);
                        return fs.throw(e::ENOMEM_journal_keys_sort);
                    }
                }
                keys.insert(keys.len(), n);
                nr_read += 1;
            }
        }
    }

    /*
     * Keys inserted before sorting, e.g. by dev_usage_init in an offline
     * device add: the main array's now
     */
    while !pre_sort.is_empty() {
        keys.reserve().map_err(|_| fs.err(e::ENOMEM_journal_keys_sort))?;
        keys.insert(keys.len(), pre_sort.pop().expect("not empty"));
    }

    sort_dedup(jk, keys);

    for i in keys.iter() {
        let k = jk.key_k(i);
        // SAFETY: a key of the entries, which are recovery's
        unsafe {
            if (*k).k.key_type() == BkeyType::btree_ptr_v2 {
                let v = (&raw mut (*k).v).cast::<c::BtreePtrV2>();
                (&raw mut (*v).repr.mem_ptr).write_unaligned(Volatile::new(0));
            }
        }
    }

    let mut msg = LogMsg::with_level(fs, if nr_extra_sorts != 0 { LogLevel::DEBUG } else { LogLevel::NOTICE });
    prt!(&mut msg, "Journal keys: {nr_read} read, {} after sorting and compacting", keys.len());
    if nr_extra_sorts != 0 {
        prt!(&mut msg, "\nRequired {nr_extra_sorts} extra sorts due to low memory");
    }
    Ok(())
}

/// The last ref's put: the keys freed, and the journal entries they're read
/// from.
#[pinned_drop]
impl PinnedDrop for c::journal_keys {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: the last ref's
        unsafe { self.reset(self.keys_mut()) };
    }
}

/// Recovery done with the keys: its ref put, and no more to be had.
pub fn journal_keys_put_initial(fs: &Fs) {
    fs.journal_keys.stop()
}

/// Btree @btree's keys at levels @level_min to @level_max, from @start to
/// @end, dropped: by recovery, rebuilding the btree from scratch.
pub fn shoot_down_journal_keys(fs: &Fs, btree: Btree, level_min: u32, level_max: u32,
                               start: Bpos, end: Bpos) {
    let Some(jk) = fs.journal_keys_get() else { return };
    let jk = jk.value();
    // SAFETY: recovery's, before anything runs alongside it
    let keys = unsafe { jk.keys_mut() };
    let s = keys.make_contiguous();
    let mut dst = 0;

    for src in 0..s.len() {
        let i = &s[src];
        if !(i.btree_id() == btree &&
             (level_min..=level_max).contains(&u32::from(i.level())) &&
             (start..=end).contains(&jk.key_pos(i))) {
            s.swap(dst, src);
            dst += 1;
        }
    }
    keys.truncate(dst);
}
