// SPDX-License-Identifier: GPL-2.0

//! The journal keys: the keys recovery read from the journal, sorted, which
//! btree lookups see overlaid on the btree until journal replay has put them
//! in it - so the btree iterator, and btree_and_journal_iter for walking a
//! node before the btree runs, read the two merged.
//!
//! Terms:
//!  - A key's index is its place in the order the keys are sorted in: btree,
//!    level - descending - and position. Equal keys sort oldest first, and
//!    sorting keeps only the newest, but for accounting keys, whose updates
//!    are deltas, each compared against the btree's version in replay.
//!  - The keys are a GapBuffer (util/gap_buffer.rs): recovery's inserts come
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
//!    them: sorting, before the btree runs; inserts, from the recovery task
//!    before anything runs alongside it, which insert_take() asserts; the
//!    last ref's put. keys_mut() is that contract.
//!  - Overwriting happens later too, from any thread, under overwrite_lock -
//!    which journal replay's commits take, in C - while the keys are read
//!    without it. So a key's overwritten flag and range are atomics, and the
//!    ranges an RcuArray the lock publishes: a reader loads a key's range
//!    index, which is stored after the range, then reads the range.
//!  - An insert bumps seq, and an iterator that finds it changed searches
//!    again for where it was - from where it started, or the
//!    btree_and_journal_iter's pos if that's later: what's before either it
//!    never returns.

use crate::accounting;
use crate::btree::bkey::{BkeyI, BkeySC, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::{btree_id_is_alloc, Fs};
use crate::locking::Guard;
use crate::util::alloc::{darray_exit, flags::GFP_KERNEL, DArrayLoan};
use crate::util::ffi::Opaque;
use crate::util::gap_buffer::GapBuffer;
use crate::util::kernel::cond_resched;
use crate::util::log::{LogLevel, LogMsg};
use crate::util::rcu::{self, ReadGuard};
use crate::{bch_err, prt};
use core::cmp::Ordering;
use core::ffi::{c_int, c_uint};
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use cstruct_macros::c_export;

use c::bch_bkey_type as kt;
use c::bch_errcode as e;
use c::bch_fs_flags as f;

type OverwriteGuard<'a> = Guard<'a, c::mutex, c::OverwriteRanges, c::OverwriteRangesLive>;

impl c::journal_key {
    fn is_overwritten(&self) -> bool {
        self.overwritten.load(Relaxed) != 0
    }

    /// Its overwrite range's index, and the range itself published.
    fn range(&self) -> u32 {
        self.overwritten_range.load(Acquire)
    }

    /// For @k, kmalloc()'d, a key of its own.
    fn new_allocated(btree: c::btree_id, level: u32, k: *mut c::bkey_i) -> Self {
        let mut n = Self::default();
        n.id = c::journal_key_id::new().with_btree_id(btree).with_level(level as u8);
        n.flags = c::journal_key_flags::new().with_allocated(true);
        n.src.allocated_k = k;
        n
    }
}

/// Of a key at @level in @btree and key @r: which btree and level sorts
/// first.
#[inline]
fn btree_cmp(btree: c::btree_id, level: u32, r: &c::journal_key) -> Ordering {
    u32::from(r.level()).cmp(&level).then(btree.0.cmp(&r.btree_id().0))
}

impl Fs {
    /// Key @k: its own, or in the journal entry it was read from -
    /// journal_key_k().
    #[inline]
    fn journal_key_k(&self, k: &c::journal_key) -> *mut c::bkey_i {
        // SAFETY: a key read from the journal is in an entry that lives as
        // long as the keys, at its offset
        unsafe {
            if k.allocated() {
                return k.src.allocated_k;
            }
            let pos = k.src.journal;
            let i = *self.journal_entries.get(pos.journal_seq_offset as usize)
                .expect("a journal key's entry");
            (&raw mut (*i).j._data).cast::<u64>().add(pos.journal_offset as usize).cast()
        }
    }

    #[inline]
    fn journal_key_pos(&self, k: &c::journal_key) -> c::bpos {
        // SAFETY: as journal_key_k()'s
        unsafe { (*self.journal_key_k(k)).k.p }
    }

    #[inline]
    fn journal_key_cmp_pos(&self, btree: c::btree_id, level: u32, pos: c::bpos, r: &c::journal_key) -> Ordering {
        btree_cmp(btree, level, r).then_with(|| pos.cmp(&self.journal_key_pos(r)))
    }

    fn journal_key_cmp(&self, l: &c::journal_key, r: &c::journal_key) -> Ordering {
        self.journal_key_cmp_pos(l.btree_id(), l.level().into(), self.journal_key_pos(l), r)
    }

    /// The index of the first key at or after @pos in @btree at @level:
    /// __bch2_journal_key_search().
    fn journal_key_search(&self, btree: c::btree_id, level: u32, pos: c::bpos) -> usize {
        let keys = self.journal_keys.keys();
        let (mut l, mut r) = (0, keys.len());

        while l < r {
            let m = l + (r - l) / 2;
            if self.journal_key_cmp_pos(btree, level, pos, &keys[m]).is_gt() {
                l = m + 1;
            } else {
                r = m;
            }
        }

        assert!(l == keys.len() || self.journal_key_cmp_pos(btree, level, pos, &keys[l]).is_le());
        assert!(l == 0 || self.journal_key_cmp_pos(btree, level, pos, &keys[l - 1]).is_gt());
        l
    }
}

impl c::journal_keys {
    pub fn keys(&self) -> &GapBuffer<c::journal_key> {
        // SAFETY: written only through keys_mut(), with nothing reading
        unsafe { &*self.keys.get() }
    }

    /// # Safety
    /// Nothing else reads or writes the keys, or pre_sort, while it's held:
    /// sorting, before the btree runs; the recovery task, before anything runs
    /// alongside it; the last ref.
    unsafe fn keys_mut(&self) -> &mut GapBuffer<c::journal_key> {
        unsafe { &mut *self.keys.get() }
    }

    /// # Safety
    /// As keys_mut().
    unsafe fn pre_sort(&self) -> DArrayLoan<'_, c::journal_key> {
        // SAFETY: the caller's; kvmalloc()'d, if allocated
        unsafe { DArrayLoan::new(&mut *self.pre_sort.get()) }
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
        k.overwritten.store(1, Relaxed);

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
                k.overwritten_range.store(r as u32, Release);
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
                k.overwritten_range.store(pr, Release);

                for i in n.start.load(Relaxed)..n.end.load(Relaxed) {
                    let ik = &keys[i];
                    assert_eq!(ik.range(), nr, "key {i} in overwrite range {nr}");
                    ik.overwritten_range.store(pr, Release);
                }
            }
            (Some((pr, p)), None) => {
                p.end.fetch_add(1, Relaxed);
                k.overwritten_range.store(pr, Release);
                if let Some(next) = next {
                    p.end.fetch_add(1, Relaxed);
                    next.overwritten_range.store(pr, Release);
                }
            }
            (None, Some((nr, n))) => {
                n.start.fetch_sub(1, Relaxed);
                k.overwritten_range.store(nr, Release);
                if let Some(prev) = prev {
                    n.start.fetch_sub(1, Relaxed);
                    prev.overwritten_range.store(nr, Release);
                }
            }
            (None, None) => unreachable!(),
        }
    }

    /// The keys freed, what they own and the buffer, and the overwrite
    /// ranges: bch2_journal_keys_reset().
    fn reset(&self, keys: &mut GapBuffer<c::journal_key>) {
        for k in keys.iter().filter(|k| k.allocated()) {
            // SAFETY: an allocated key's own, freed with it
            unsafe { c::rust_kfree(k.src.allocated_k.cast()) };
        }
        keys.free_storage();

        let mut g = self.overwrites.lock();
        g.overwrites_nr = 0;
        g.published().ranges.clear();
    }
}

// ── Lookups, for the btree iterator ──────────────────────────────────────

/// The first key from @pos to @end, not overwritten: searched for, or from
/// *@idx if it's close, and *@idx left at it for the next.
fn peek_max(fs: &Fs, btree: c::btree_id, level: u32, pos: c::bpos, end: c::bpos, idx: &mut usize)
    -> *const c::bkey_i
{
    let jk = &fs.journal_keys;
    let keys = jk.keys();
    let mut iters = 0;

    assert!(*idx <= keys.len(), "journal key {} of {}", *idx, keys.len());
    'search: loop {
        if *idx == 0 {
            *idx = fs.journal_key_search(btree, level, pos);
        }

        while *idx > 0 && fs.journal_key_cmp_pos(btree, level, end, &keys[*idx - 1]).is_le() {
            *idx -= 1;
            iters += 1;
            if iters == 10 {
                *idx = 0;
                continue 'search;
            }
        }

        let rcu = rcu::read_lock();
        while let Some(k) = keys.get(*idx) {
            if fs.journal_key_cmp_pos(btree, level, end, k).is_lt() {
                break;
            }

            if k.is_overwritten() {
                *idx = match k.range() {
                    0 => *idx + 1,
                    r => jk.overwrite_range(r, &rcu).end.load(Relaxed),
                };
                continue;
            }

            if fs.journal_key_cmp_pos(btree, level, pos, k).is_le() {
                return fs.journal_key_k(k);
            }

            *idx += 1;
            iters += 1;
            if iters == 10 {
                *idx = 0;
                continue 'search;
            }
        }
        return ptr::null();
    }
}

#[c_export]
pub fn bch2_journal_keys_peek_max(c: &Opaque<c::bch_fs>, btree: c::btree_id, level: c_uint, pos: c::bpos,
                                  end: c::bpos, idx: &mut usize) -> *const c::bkey_i {
    peek_max(Fs::from_c(c), btree, level, pos, end, idx)
}

/// peek_max(), backwards: the last key from @pos down to @end.
#[c_export]
pub fn bch2_journal_keys_peek_prev_min(c: &Opaque<c::bch_fs>, btree: c::btree_id, level: c_uint, pos: c::bpos,
                                       end: c::bpos, idx: &mut usize) -> *const c::bkey_i {
    let fs = Fs::from_c(c);
    let jk = &fs.journal_keys;
    let keys = jk.keys();
    let mut iters = 0;

    assert!(*idx <= keys.len(), "journal key {} of {}", *idx, keys.len());
    if keys.is_empty() {
        return ptr::null();
    }
    'search: loop {
        if *idx == 0 {
            *idx = fs.journal_key_search(btree, level, pos);
        }

        while *idx < keys.len() && fs.journal_key_cmp_pos(btree, level, end, &keys[*idx]).is_ge() {
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
            if fs.journal_key_cmp_pos(btree, level, end, k).is_gt() {
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

            if fs.journal_key_cmp_pos(btree, level, pos, k).is_ge() {
                return fs.journal_key_k(k);
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
        return ptr::null();
    }
}

#[c_export]
pub fn bch2_journal_keys_peek_slot(c: &Opaque<c::bch_fs>, btree: c::btree_id, level: c_uint, pos: c::bpos)
    -> *const c::bkey_i
{
    peek_max(Fs::from_c(c), btree, level, pos, pos, &mut 0)
}

/// Whether the journal deletes @pos, before replay's done.
///
/// # Safety
/// @trans is live.
#[c_export]
pub unsafe fn bch2_key_deleted_in_journal(trans: *mut c::btree_trans, btree: c::btree_id, level: c_uint,
                                          pos: c::bpos) -> bool {
    // SAFETY: the caller's
    let trans = unsafe { &*trans };
    if !trans.bits.journal_replay_not_finished() {
        return false;
    }

    // SAFETY: trans->c outlives it
    let fs = unsafe { Fs::borrow_raw(trans.c) };
    let Some(k) = fs.journal_keys.keys().get(fs.journal_key_search(btree, level, pos)) else { return false };
    if k.btree_id() != btree || u32::from(k.level()) != level {
        return false;
    }

    // SAFETY: as journal_key_k()'s
    let k = unsafe { &*fs.journal_key_k(k) };
    k.k.p == pos && k.k.key_type() == kt::KEY_TYPE_deleted
}

/// The key at @pos overwritten, if the journal has one that isn't - unless
/// @check. The caller holds overwrite_lock.
#[c_export]
pub fn bch2_journal_key_check_or_overwrite(c: &Opaque<c::bch_fs>, btree: c::btree_id, level: c_uint,
                                           pos: c::bpos, check: bool) -> c_int {
    let fs = Fs::from_c(c);
    let jk = &fs.journal_keys;
    // SAFETY: the caller holds it, through this call
    let mut g = unsafe { jk.overwrites.held() };
    let idx = fs.journal_key_search(btree, level, pos);

    let Some(k) = jk.keys().get(idx) else { return 0 };
    if k.btree_id() != btree || u32::from(k.level()) != level || k.is_overwritten() || fs.journal_key_pos(k) != pos {
        return 0;
    }

    /* C's -1 for an overwritten key, which returned above */
    if check {
        return 0;
    }

    jk.overwrite(&mut g, idx);
    0
}

// ── Inserts, by recovery before the btree's read-write ───────────────────

/// @k inserted, as @btree and @level's key, replacing an equal key - adding
/// to it, for accounting - or before it. Taking @k, whose kfree() is the
/// keys' from here on. Before the btree runs, into pre_sort, which sorting
/// merges in.
///
/// # Safety
/// @k is a key, kmalloc()'d, the caller's; still the caller's on error.
unsafe fn insert_take(fs: &Fs, btree: c::btree_id, level: u32, k: *mut c::bkey_i) -> Result<(), BchError> {
    let jk = &fs.journal_keys;
    let n = c::journal_key::new_allocated(btree, level, k);

    if !fs.flag(f::BCH_FS_btree_running) {
        // SAFETY: before the btree runs, the keys are recovery's alone
        return unsafe { jk.pre_sort() }.push(n, GFP_KERNEL)
            .map_err(|_| fs.err(e::BCH_ERR_ENOMEM_journal_key_insert));
    }

    // SAFETY: the caller's
    let k = unsafe { &mut *BkeyI::from_raw_mut(k) };
    let idx = fs.journal_key_search(btree, level, k.k.p);

    assert!(!fs.flag(f::BCH_FS_may_go_rw));
    assert!(!fs.flag(f::BCH_FS_rw));
    /* keys are frozen for the scrub, whose reads run on workqueues */
    assert!(!fs.flag(f::BCH_FS_scrub_journal));
    /* userspace's current is C's __thread variable, which Rust can't name */
    #[cfg(kernel)]
    assert!(fs.recovery_task == unsafe { c::get_current() });

    // SAFETY: from the recovery task, before anything runs alongside it, as
    // the asserts say
    let keys = unsafe { jk.keys_mut() };

    if let Some(o) = keys.get_mut(idx).filter(|o| fs.journal_key_cmp(&n, o).is_eq()) {
        // SAFETY: as journal_key_k()'s
        let ok = unsafe { &*BkeyI::from_raw(fs.journal_key_k(o)) };
        let accounting = k.k.key_type() == kt::KEY_TYPE_accounting && ok.k.key_type() == kt::KEY_TYPE_accounting;

        if !(accounting && !o.allocated()) {
            if accounting {
                accounting::accumulate(k, ok);
            }
            if o.allocated() {
                // SAFETY: the replaced key's own
                unsafe { c::rust_kfree(o.src.allocated_k.cast()) };
            }
            *o = n;
            return Ok(());
        }
    }

    if keys.reserve().is_err() {
        bch_err!(fs, "bch2_journal_key_insert_take: error allocating new key array (size {})",
                 keys.size.max(8) * 2);
        return fs.throw(e::BCH_ERR_ENOMEM_journal_key_insert);
    }
    keys.insert(idx, n);
    jk.seq.fetch_add(1, Relaxed);
    Ok(())
}

/// insert_take() of a copy of @k.
fn insert(fs: &Fs, btree: c::btree_id, level: u32, k: &BkeyI) -> Result<(), BchError> {
    let bytes = k.k.bytes();
    let n = crate::util::alloc::kmalloc(bytes, GFP_KERNEL).map_err(|_| fs.err(e::BCH_ERR_ENOMEM_journal_key_insert))?;
    // SAFETY: @bytes of key, into as many of ours; then ours to hand over, or
    // free if it isn't taken
    unsafe {
        ptr::copy_nonoverlapping(k.as_ffi().cast::<u8>(), n.as_ptr(), bytes);
        insert_take(fs, btree, level, n.as_ptr().cast())
            .inspect_err(|_| c::rust_kfree(n.as_ptr().cast()))
    }
}

/// # Safety
/// As insert_take()'s.
#[c_export]
pub unsafe fn bch2_journal_key_insert_take(c: &Opaque<c::bch_fs>, btree: c::btree_id, level: c_uint,
                                           k: *mut c::bkey_i) -> c_int {
    ret_to_c(unsafe { insert_take(Fs::from_c(c), btree, level, k) })
}

/// Only from the recovery thread while we're still RO - once we're RW, the
/// keys are read from multiple threads.
///
/// # Safety
/// @k is a key, all of its u64s there.
#[c_export]
pub unsafe fn bch2_journal_key_insert(c: &Opaque<c::bch_fs>, btree: c::btree_id, level: c_uint,
                                      k: *const c::bkey_i) -> c_int {
    // SAFETY: the caller's
    ret_to_c(insert(Fs::from_c(c), btree, level, unsafe { &*BkeyI::from_raw(k) }))
}

#[c_export]
pub fn bch2_journal_key_delete(c: &Opaque<c::bch_fs>, btree: c::btree_id, level: c_uint, pos: c::bpos) -> c_int {
    let mut whiteout = c::bkey_i { k: c::bkey::new(), ..Default::default() };
    whiteout.k.p = pos;
    // SAFETY: a key with no value, all of it there
    ret_to_c(insert(Fs::from_c(c), btree, level, unsafe { &*BkeyI::from_raw(&whiteout) }))
}

// ── Iterating: a btree node's keys, with the journal's overlaid ──────────

impl c::journal_iter {
    fn new(fs: &Fs, btree: c::btree_id, level: u32, pos: c::bpos) -> Self {
        Self {
            btree_id: btree,
            level,
            idx: fs.journal_key_search(btree, level, pos),
            seq: fs.journal_keys.seq(),
            pos,
        }
    }

    fn verify(&self, fs: &Fs) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            if let Some(k) = fs.journal_keys.keys().get(self.idx) {
                assert!(btree_cmp(self.btree_id, self.level, k).is_le(), "journal iter before its btree");
            }
        }
    }

    fn advance(&mut self, fs: &Fs) {
        if self.idx < fs.journal_keys.keys().len() {
            self.idx += 1;
        }
    }

    /// The next key not overwritten, if it's in the iterator's btree and
    /// level; after an insert, the first from @pos or where the iterator
    /// started, whichever's later.
    fn peek(&mut self, fs: &Fs, pos: c::bpos) -> *const c::bkey_i {
        let jk = &fs.journal_keys;
        let seq = jk.seq();
        if self.seq != seq {
            self.idx = fs.journal_key_search(self.btree_id, self.level, pos.max(self.pos));
            self.seq = seq;
        }
        self.verify(fs);

        let keys = jk.keys();
        let rcu = rcu::read_lock();
        while let Some(k) = keys.get(self.idx) {
            match btree_cmp(self.btree_id, self.level, k) {
                Ordering::Less    => break,
                Ordering::Greater => panic!("journal iter past its btree"),
                Ordering::Equal   => {}
            }

            if !k.is_overwritten() {
                return fs.journal_key_k(k);
            }

            self.idx = match k.range() {
                0 => self.idx + 1,
                r => jk.overwrite_range(r, &rcu).end.load(Relaxed),
            };
        }
        ptr::null()
    }
}

impl c::btree_and_journal_iter {
    fn replay_not_finished(&self) -> bool {
        // SAFETY: the iterator's transaction outlives it
        unsafe { (*self.trans).bits.journal_replay_not_finished() }
    }

    fn advance(&mut self) {
        if self.pos == SPOS_MAX {
            self.at_end = true;
        } else {
            self.pos = self.pos.successor();
        }
    }

    /// The next few keys' nodes read ahead, from a copy of the iterator.
    fn prefetch(&self, fs: &Fs) {
        let mut iter = *self;
        let level = iter.journal.level;
        let nr = if fs.flag(f::BCH_FS_started) { if level > 1 { 0 } else { 2 } } else if level > 1 { 1 } else { 16 };

        iter.prefetch = false;
        iter.fail_if_too_many_whiteouts = true;

        let mut tmp = BkeyBuf::new();
        for _ in 0..nr {
            iter.advance();
            let k = iter.peek(fs);
            if k.k.is_null() {
                break;
            }

            // SAFETY: a key from the iterator, valid until it moves
            tmp.reassemble(unsafe { BkeySC::from_c(&k) });
            // SAFETY: the iterator's transaction
            unsafe {
                c::bch2_btree_node_prefetch(iter.trans, ptr::null_mut(), AsRef::<BkeyI>::as_ref(&tmp).as_ffi(),
                                            iter.journal.btree_id, level - 1);
            }
        }
    }

    /// The next key at or after pos, the btree node's or the journal's,
    /// whichever's first - the journal's if both - skipping whiteouts.
    fn peek(&mut self, fs: &Fs) -> c::bkey_s_c {
        let null = c::bkey_s_c { k: ptr::null_mut(), v: ptr::null_mut() };
        let mut iters = 0;

        if self.prefetch && self.journal.level != 0 {
            self.prefetch(fs);
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
                // SAFETY: the node's iterator, of node b
                let k = unsafe { c::bch2_btree_node_iter_peek_unpack(&mut self.node_iter, self.b, &mut self.unpacked) };
                // SAFETY: a key of the node's
                if k.k.is_null() || unsafe { (*k.k).p } >= self.pos {
                    break k;
                }
                unsafe { c::bch2_btree_node_iter_advance(&mut self.node_iter, self.b) };
            };

            let mut journal_k = ptr::null();
            if self.replay_not_finished() {
                journal_k = loop {
                    let k = self.journal.peek(fs, self.pos);
                    // SAFETY: a journal key
                    if k.is_null() || unsafe { (*k).k.p } >= self.pos {
                        break k;
                    }
                    self.journal.advance(fs);
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

            // SAFETY: the node, and its data, while the iterator's on it
            if !ret.k.is_null() && !self.b.is_null() && unsafe { (*ret.k).p > (*(*self.b).data).max_key } {
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
            if k.key_type() != kt::KEY_TYPE_deleted {
                return ret;
            }
            self.advance();
        }
    }

    /// Over node @b's keys, from @node_iter, and the journal's from @pos.
    ///
    /// # Safety
    /// @trans and @b are live, @b locked, for as long as the iterator.
    unsafe fn new(trans: *mut c::btree_trans, b: *mut c::btree, node_iter: c::btree_node_iter, pos: c::bpos)
        -> Self
    {
        let mut iter = Self {
            trans,
            b,
            node_iter,
            // SAFETY: the caller's
            pos: unsafe { (*(*b).data).min_key },
            ..Default::default()
        };

        if iter.replay_not_finished() {
            // SAFETY: the caller's
            let (fs, btree, level) = unsafe { (Fs::borrow_raw((*trans).c), (*b).c.btree_id, (*b).c.level) };
            iter.journal = c::journal_iter::new(fs, c::btree_id(btree.into()), level.into(), pos);
        }
        iter
    }
}

#[c_export]
pub fn bch2_btree_and_journal_iter_advance(iter: &mut c::btree_and_journal_iter) {
    iter.advance()
}

#[c_export]
pub fn bch2_btree_and_journal_iter_peek(c: &Opaque<c::bch_fs>, iter: &mut c::btree_and_journal_iter)
    -> c::bkey_s_c
{
    iter.peek(Fs::from_c(c))
}

/// # Safety
/// As btree_and_journal_iter::new()'s; @iter is the caller's to overwrite.
#[c_export]
pub unsafe fn __bch2_btree_and_journal_iter_init_node_iter(trans: *mut c::btree_trans,
                                                          iter: *mut c::btree_and_journal_iter,
                                                          b: *mut c::btree, node_iter: c::btree_node_iter,
                                                          pos: c::bpos) {
    // SAFETY: the caller's
    unsafe { iter.write(c::btree_and_journal_iter::new(trans, b, node_iter, pos)) }
}

/// From the node's first key: what btree_gc and check_topology walk with,
/// before the filesystem is read-write.
///
/// # Safety
/// As __bch2_btree_and_journal_iter_init_node_iter()'s.
#[c_export]
pub unsafe fn bch2_btree_and_journal_iter_init_node_iter(trans: *mut c::btree_trans,
                                                        iter: *mut c::btree_and_journal_iter,
                                                        b: *mut c::btree) {
    let mut node_iter = c::btree_node_iter::default();
    // SAFETY: the caller's
    unsafe {
        c::bch2_btree_node_iter_init_from_start(&mut node_iter, b);
        iter.write(c::btree_and_journal_iter::new(trans, b, node_iter, (*(*b).data).min_key));
    }
}

// ── Sorting, and the keys' lifetime ───────────────────────────────────────

/// Journal keys @l and @r in sort order: of equal keys, the oldest first - of
/// two a rewind undoes, the newest - and what was read before what was
/// inserted.
fn sort_cmp(fs: &Fs, l: &c::journal_key, r: &c::journal_key) -> Ordering {
    fs.journal_key_cmp(l, r).then_with(|| {
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
fn sort_dedup(fs: &Fs, keys: &mut GapBuffer<c::journal_key>) {
    let s = keys.make_contiguous();
    let mut cmps = 0u32;
    s.sort_unstable_by(|l, r| {
        cmps = cmps.wrapping_add(1);
        if cmps % 1024 == 0 {
            cond_resched();
        }
        sort_cmp(fs, l, r)
    });

    cond_resched();

    let mut dst = 0;
    for src in 0..s.len() {
        // SAFETY: as journal_key_k()'s
        let accounting = unsafe { (*fs.journal_key_k(&s[src])).k.key_type() } == kt::KEY_TYPE_accounting;
        if !accounting && src + 1 < s.len() && fs.journal_key_cmp(&s[src], &s[src + 1]).is_eq() {
            continue;
        }
        s.swap(dst, src);
        dst += 1;
    }
    keys.truncate(dst);
}

fn journal_seq_is_rewound(fs: &Fs, seq: u64) -> bool {
    let ranges = &fs.journal.rewind_ranges;
    // SAFETY: a darray: nr ranges at data
    let ranges = if ranges.nr == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(ranges.data, ranges.nr) } };
    ranges.iter().any(|r| seq > r.to && seq <= r.from)
}

/// The keys, from the journal entries read - again, if they were already,
/// as for a rewind recovery decides on after a scrub of the recent entries.
fn keys_sort(fs: &Fs) -> Result<(), BchError> {
    let jk = &fs.journal_keys;
    // SAFETY: before the btree runs, the keys are recovery's alone
    let keys = unsafe { jk.keys_mut() };
    let (mut nr_read, mut nr_extra_sorts) = (0usize, 0usize);
    let start_seq = if fs.journal_scrub_seq != 0 { fs.journal_scrub_seq } else { fs.journal_replay_seq_start };

    jk.reset(keys);

    for (_, &i) in fs.journal_entries.iter_from(0) {
        // SAFETY: an entry the journal read, valid until the last put
        let Some(i) = (unsafe { i.as_ref() }) else { continue };
        if i.ignore_blacklisted || i.ignore_not_dirty {
            continue;
        }

        cond_resched();

        // SAFETY: as i's
        let j = unsafe { c::journal_replay::jset(i) };
        let seq = j.seq.get();
        let seq_rewound = journal_seq_is_rewound(fs, seq);

        if seq_rewound && !j.has_overwrites() {
            bch_err!(fs, "cannot rewind journal seq {seq}: no overwrite entries (journal_transaction_names was off)");
            return fs.throw(e::BCH_ERR_journal_rewind_no_overwrites);
        }

        for entry in j.entries() {
            let rewind = seq_rewound && entry.level == 0 && !btree_id_is_alloc(entry.btree_id.into());
            let want = if rewind {
                c::bch_jset_entry_type::BCH_JSET_ENTRY_overwrite
            } else {
                c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_keys
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
                    .with_btree_id(c::btree_id(entry.btree_id.into()))
                    .with_level(entry.level);
                n.flags = c::journal_key_flags::new().with_rewind(rewind);
                n.src.journal = c::journal_key_journal_pos {
                    journal_seq_offset: (seq - fs.journal_entries_base_seq) as u32,
                    journal_offset: ((k.as_bytes().as_ptr() as usize - (&raw const i.j._data) as usize)
                                     / size_of::<u64>()) as u32,
                };

                if keys.reserve().is_err() {
                    sort_dedup(fs, keys);
                    nr_extra_sorts += 1;

                    if keys.len() * 8 > keys.size * 7 {
                        bch_err!(fs, "Too many journal keys for slowpath; have {} compacted, buf size {}, \
                                      processed {nr_read} keys at seq {seq}", keys.len(), keys.size);
                        return fs.throw(e::BCH_ERR_ENOMEM_journal_keys_sort);
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
    // SAFETY: as keys'
    let mut pre_sort = unsafe { jk.pre_sort() };
    while !pre_sort.is_empty() {
        keys.reserve().map_err(|_| fs.err(e::BCH_ERR_ENOMEM_journal_keys_sort))?;
        keys.insert(keys.len(), pre_sort.pop().expect("not empty"));
    }
    drop(pre_sort);

    sort_dedup(fs, keys);

    for i in keys.iter() {
        let k = fs.journal_key_k(i);
        // SAFETY: a key of the entries, which are recovery's
        unsafe {
            if (*k).k.key_type() == kt::KEY_TYPE_btree_ptr_v2 {
                let v = (&raw mut (*k).v).cast::<c::BtreePtrV2>();
                (&raw mut (*v).repr.mem_ptr).write_unaligned(0);
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

#[c_export]
pub fn bch2_journal_keys_sort(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(keys_sort(Fs::from_c(c)))
}

/// A ref on the keys dropped: the last frees them, and the journal entries
/// they're read from.
#[c_export]
pub fn bch2_journal_keys_put(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    let jk = &fs.journal_keys;

    assert!(jk.ref_.load(Relaxed) > 0, "journal keys put without a ref");
    if jk.ref_.fetch_sub(1, AcqRel) != 1 {
        return;
    }

    // SAFETY: the last ref's
    unsafe {
        jk.reset(jk.keys_mut());

        for k in jk.pre_sort().iter() {
            c::rust_kfree(k.src.allocated_k.cast());
        }
        darray_exit(&mut *jk.pre_sort.get());

        let entries = &mut (*fs.as_ptr()).journal_entries;
        for (_, &i) in entries.iter_from(0) {
            c::rust_kvfree(i.cast());
        }
        entries.free();
    }
}

/// Btree @btree's keys at levels @level_min to @level_max, from @start to
/// @end, dropped: by recovery, rebuilding the btree from scratch.
pub fn shoot_down_journal_keys(fs: &Fs, btree: c::btree_id, level_min: u32, level_max: u32,
                               start: c::bpos, end: c::bpos) {
    // SAFETY: recovery's, before anything runs alongside it
    let keys = unsafe { fs.journal_keys.keys_mut() };
    let s = keys.make_contiguous();
    let mut dst = 0;

    for src in 0..s.len() {
        let i = &s[src];
        if i.btree_id() == btree &&
           (level_min..=level_max).contains(&u32::from(i.level())) &&
           (start..=end).contains(&fs.journal_key_pos(i)) {
            if i.allocated() {
                // SAFETY: its own, dropped with it
                unsafe { c::rust_kfree(i.src.allocated_k.cast()) };
            }
        } else {
            s.swap(dst, src);
            dst += 1;
        }
    }
    keys.truncate(dst);
}

#[c_export]
pub fn bch2_shoot_down_journal_keys(c: &Opaque<c::bch_fs>, btree: c::btree_id, level_min: c_uint,
                                    level_max: c_uint, start: c::bpos, end: c::bpos) {
    shoot_down_journal_keys(Fs::from_c(c), btree, level_min, level_max, start, end)
}
