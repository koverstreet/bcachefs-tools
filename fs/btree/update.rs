// SPDX-License-Identifier: GPL-2.0

//! Btree updates: queueing a transaction's, and the helpers that make them.

#![allow(non_camel_case_types)]

use crate::alloc::capacity::DiskReservation;
use crate::btree::{BtreeNodeType, journal_overlay};
use crate::btree::bkey::{BkeyV, bkey_maybe_mergable, bkey_eq, bkey_gt, bkey_le, bkey_lt,
    bkey_merge, BkeyI, BkeyS, BkeySC, BkeyWhole, MutBkey, ValidatedVal, BKEY_U64S};
use crate::btree::iter::{btree_id_cached, commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
                         CommitOpts, TransAttempt, TransBkey, UpdateTriggerFlags, Watermark};
use crate::btree::path::{BtreePath, KeyVal, PathIdx, Paths};
use crate::btree::write_buffer::insert_checks;
use crate::journal::journal::JournalFlag;
use crate::data::{durability::BkeyVDurability, extents::{cut_back_s, cut_front_s}};
use crate::debug::trace::events;
use crate::prt;
use crate::snapshots::snapshot::{has_overwrites, is_ancestor};
use core::cmp::Ordering;
use crate::errcode::{Errcode, BchError};
use crate::fs::{Fs, FsFlag};
use crate::os::kernel::unlikely;
use crate::util::vstructs::{plain_bytes, Container, DstHead, EntryWalk, Head, Plain, SelfSized, Unsized,
                            Validated, ValidatedEntries, ValidatedEntriesMut, ViewEntries, ViewEntriesMut};
use crate::util::{Printbuf, ToText};
use core::cell::Cell;
use core::ffi::{c_ulong, c_uint};
use core::marker::PhantomData;
use core::mem::size_of;
use core::ops::Range;
use core::ptr::NonNull;
use cstruct_macros::{bch_enum, MaybeZeroable, bitfield, BitfieldMembers};
use crate::format::{Bpos, Btree, BkeyType, MetadataVersion, Val, Bkey, Deleted, JsetEntry, Set, JsetEntryType, BtreeFlags};
use zerocopy::IntoBytes;
use nestify::nest;

/* Transaction types: */
nest! {
    #[repr(C)]
    #[derive(Clone, Copy, MaybeZeroable, BitfieldMembers)]
    pub struct BtreeInsertEntry {
        pub flags: c_uint,
        pub sort_order: u8,
        pub bkey_type: u8,
        #[members]
        #>[bitfield(u16)]
        pub bits: pub struct BtreeInsertEntryBits {
            #[bits(8)] pub btree_id: Btree,
            level:                     u3,
            cached:                    bool,
            insert_trigger_run:        bool,
            overwrite_trigger_run:     bool,
            key_cache_already_flushed: bool,
            key_cache_flushing:        bool,
        },
        /*
         * @old_k may be a key from the journal or the key cache;
         * @old_btree_u64s always refers to the size of the key being
         * overwritten in the btree:
         */
        pub old_btree_u64s: u8,
        k_buf_u64s: u8,
        pub path: PathIdx,
        k: *mut BkeyI<Val>,
        /* key being overwritten: */
        pub old_k: Bkey,
        pub old_v: *const Val,
        pub ip_allocated: c_ulong,
    }
}

#[repr(C)]
#[derive(Clone, Copy, MaybeZeroable)]
pub struct BtreeTransCommitHook {
    pub fn_: Option<unsafe fn(&TransAttempt<'_, '_>, *mut BtreeTransCommitHook) -> Result<(), BchError>>,
    pub next: *mut BtreeTransCommitHook,
}

/*
 * The node types whose updates run triggers, a bit each: a leaf's node type
 * is its btree's id + 1. Every snapshotted btree runs transactional triggers,
 * so that bch2_trigger_snapshot_nr_keys() runs for its keys; and interior
 * nodes do, for their pointers.
 */
const TRANS_TRIGGERS: u64 = 1 << BtreeNodeType::btree.0 |
    crate::btrees_with(BtreeFlags::trans_triggers.union(BtreeFlags::snapshots)) << 1;
const ATOMIC_TRIGGERS: u64 = crate::btrees_with(BtreeFlags::atomic_triggers) << 1;

/// An update's node type - which may be a btree's this code doesn't know - in
/// @mask.
const fn node_type_in(bkey_type: u8, mask: u64) -> bool {
    bkey_type < 64 && mask & (1 << bkey_type) != 0
}

/// Where an update's key is, and its room: a TransBkey's - so the key is as
/// good as one is, until the attempt's over - or an update's already queued.
#[derive(Clone, Copy)]
pub(crate) struct QueuedKey {
    k:        NonNull<BkeyI<Val>>,
    buf_u64s: u8,
}

impl QueuedKey {
    pub(crate) fn as_ptr(self) -> *mut BkeyI<Val> {
        self.k.as_ptr()
    }
}

impl<V: ?Sized> From<&mut TransBkey<'_, '_, V>> for QueuedKey {
    fn from(k: &mut TransBkey<'_, '_, V>) -> Self {
        let (k, buf_u64s) = k.as_raw();
        QueuedKey { k, buf_u64s: buf_u64s.min(u8::MAX.into()) as u8 }
    }
}

impl BtreeInsertEntry {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(key: QueuedKey, path: PathIdx, btree: Btree, level: u32, cached: bool,
                      flags: c_uint, ip: c_ulong) -> Self {
        let mut bits = BtreeInsertEntryBits::default();
        bits.set_btree_id(btree);
        bits.set_level(level as u8);
        bits.set_cached(cached);

        BtreeInsertEntry {
            flags,
            sort_order:     btree.trigger_order(),
            bkey_type:      BtreeNodeType::of(level, btree).0 as u8,
            bits,
            old_btree_u64s: 0,
            k_buf_u64s:     key.buf_u64s,
            path,
            k:              key.k.as_ptr(),
            old_k:          Bkey::new(),
            old_v:          core::ptr::null(),
            ip_allocated:   ip,
        }
    }

    pub(crate) fn key(&self) -> &BkeyI {
        unsafe { &*BkeyI::from_raw(self.k) }
    }

    /// The key, for a trigger to change in place: it may be one the update's
    /// caller lent.
    pub(crate) fn key_ptr(&self) -> *mut BkeyI<Val> {
        self.k
    }

    pub(crate) fn key_buf_u64s(&self) -> u32 {
        self.k_buf_u64s.into()
    }

    pub(crate) fn queued_key(&self) -> QueuedKey {
        QueuedKey { k: NonNull::new(self.k).expect("an update of no key"), buf_u64s: self.k_buf_u64s }
    }

    pub(crate) fn set_key(&mut self, key: QueuedKey) {
        self.k          = key.k.as_ptr();
        self.k_buf_u64s = key.buf_u64s;
    }

    pub const fn has_trans_triggers(&self) -> bool {
        node_type_in(self.bkey_type, TRANS_TRIGGERS)
    }

    pub const fn has_atomic_triggers(&self) -> bool {
        node_type_in(self.bkey_type, ATOMIC_TRIGGERS)
    }

    pub const fn has_triggers(&self) -> bool {
        node_type_in(self.bkey_type, TRANS_TRIGGERS | ATOMIC_TRIGGERS)
    }
}


#[bch_enum(closed)]
#[mask(TransCommitFlags: u32)]
#[repr(u32)]
pub enum TransCommitFlagBit {
    /* First bits for Watermark: */
    /// don't check for enospc
    no_enospc = crate::alloc::foreground::BCH_WATERMARK_BITS + 1,
    /// don't attempt to take a ref on c->writes
    no_check_rw,
    /// don't take a journal reservation, instead pin journal entry
    /// referred to by trans->JournalRes.seq
    no_journal_res,
    /// don't drop noop updates
    no_skip_noops,
    /// operation required for journal reclaim; may return error instead
    /// of deadlocking if BCH_WATERMARK_reclaim not specified
    journal_reclaim,
    /// in journal replay
    journal_replay,
    /// we're in journal replay - accounting updates have already been
    /// applied
    skip_accounting_apply,
}

/// The whiteout that deletes @k in @btree. A KEY_TYPE_extent_whiteout says
/// there's no extent there: key start positions, extent_whiteouts included -
/// KEY_TYPE_whiteouts aren't - only go up. It's a deletion's in a leaf
/// snapshot of an extents btree, once the filesystem has the feature, which
/// this asks for; a snapshot the table doesn't have is a leaf, as C took it.
/// Any other deletion's is KEY_TYPE_whiteout.
pub fn extent_whiteout_type(fs: &Fs, btree: Btree, k: &Bkey) -> BkeyType {
    if btree.is_extents_snapshots() &&
       k.key_type().is_deleted() &&
       fs.snapshots.is_leaf(k.p.snapshot) != Ok(false) &&
       fs.request_incompat_feature(MetadataVersion::extent_snapshot_whiteouts).is_ok() {
        BkeyType::extent_whiteout
    } else {
        BkeyType::whiteout
    }
}

fn insert_entry_cmp(l: &BtreeInsertEntry, r: &BtreeInsertEntry) -> Ordering {
    l.sort_order.cmp(&r.sort_order)
        .then(l.bits.cached().cmp(&r.bits.cached()))
        .then(r.bits.level().cmp(&l.bits.level()))
        .then_with(|| l.key().k.p.cmp(&r.key().k.p))
}

/*
 * bch2_btree_path_peek_slot() for a cached iterator might return a key in a
 * different snapshot:
 */
pub(crate) fn peek_slot_exact(path: &BtreePath, u: &mut Bkey) -> *const Val {
    match path.peek_slot(u) {
        Some(KeyVal(v)) if u.p == path.pos => v,
        _ => {
            *u = Bkey::new();
            u.p = path.pos;
            core::ptr::null()
        }
    }
}

impl Fs {
    fn log_msg_with(&self, flags: CommitOpts, msg: &dyn ToText) -> Result<(), BchError> {
        let mut buf = Printbuf::new();
        msg.to_text(&mut buf);

        if buf.allocation_failure() {
            return Err(Errcode::ENOMEM_trans_log_msg.into());
        }

        if !self.journal.flags.get(JournalFlag::running) {
            self.journal.early_entry_add(JsetEntryType::log, 0, 1, buf.as_bytes())
        } else {
            let trans = crate::btree_trans!(self);
            commit_do(&trans, None, flags, |t| t.log_msg(&buf))
        }
    }

    /*
     * bch_btree_delete_range - delete everything within a given range
     *
     * Range is a half open interval - [start, end)
     */
    pub fn btree_delete_range(
        &self,
        btree_id:     Btree,
        start:        Bpos,
        end:          Bpos,
        iter_flags:   BtreeIterFlags,
        update_flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let trans = crate::btree_trans!(self);
        match trans.delete_range_flags(btree_id, start, end, iter_flags.bits() | update_flags.bits()) {
            Err(e) if e.matches(Errcode::transaction_restart_nested) => Ok(()),
            r => r,
        }
    }

    /*
     * bch2_btree_insert - insert keys into the extent btree
     * @c:			pointer to struct bch_fs
     * @id:			btree to insert into
     * @k:			key to insert
     * @disk_res:		must be non-NULL whenever inserting or potentially
     *			splitting data extents
     * @commit_flags:	transaction commit flags
     * @iter_flags:		btree iter update trigger flags
     *
     * Returns:		0 on success, error code on failure
     */
    pub fn btree_insert(
        &self,
        btree_id:     Btree,
        key:          impl AsRef<BkeyI>,
        disk_res:     Option<&DiskReservation<'_>>,
        commit_flags: impl Into<CommitOpts>,
        iter_flags:   BtreeIterFlags,
    ) -> Result<(), BchError> {
        self.btree_insert_k_i(btree_id, key.as_ref(), disk_res, commit_flags.into(), iter_flags.bits())
    }

    /// Not monomorphized per key type
    fn btree_insert_k_i(
        &self,
        btree_id:     Btree,
        key:          &BkeyI,
        disk_res:     Option<&DiskReservation<'_>>,
        commit_flags: CommitOpts,
        iter_flags:   u32,
    ) -> Result<(), BchError> {
        let trans = crate::btree_trans!(self);
        commit_do(&trans, disk_res, commit_flags,
                  |t| t.insert_raw(btree_id, t.bkey_copy(key)?, iter_flags))
    }

    pub fn fs_log_msg(&self, msg: &dyn ToText) -> Result<(), BchError> {
        self.log_msg_with(CommitOpts::new(), msg)
    }

    /*
     * Use for logging messages during recovery to enable reserved space and avoid
     * blocking.
     */
    pub fn journal_log_msg(&self, msg: &dyn ToText) -> Result<(), BchError> {
        self.log_msg_with(CommitOpts::new().watermark(Watermark::reclaim), msg)
    }
}

// ── A transaction's updates, and log entries with its commit ────────────

/*
 * Its entries are written only here, each whole - subbuf_push() makes each as
 * long as its header says, and journal_entry_add()'s hold keys - so they're
 * walked as validated, as C walks them.
 */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct BtreeTransSubbuf<T: ?Sized> {
    base:     u16,
    u64s:     u16,
    size:     u16,
    _entries: PhantomData<T>,
}

impl<T: ?Sized> Clone for BtreeTransSubbuf<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for BtreeTransSubbuf<T> {}

impl<T: ?Sized> Default for BtreeTransSubbuf<T> {
    fn default() -> Self {
        Self { base: 0, u64s: 0, size: 0, _entries: PhantomData }
    }
}

impl<T: ?Sized> BtreeTransSubbuf<T> {
    pub fn u64s(&self) -> usize {
        usize::from(self.u64s)
    }

    /// Its entries, for @f: growing it moves it, so they're only lent for the
    /// call.
    #[inline]
    pub fn with<R>(self, trans: &BtreeTrans<'_>, f: impl FnOnce(&Subbuf<T>) -> R) -> R {
        let p = trans.mem_with(|m| m.range(usize::from(self.base), usize::from(self.u64s)));
        f(unsafe { &*(p.as_ptr() as *const Subbuf<T>) })
    }

    #[inline]
    fn with_mut<R>(self, trans: &BtreeTrans<'_>, f: impl FnOnce(&mut Subbuf<T>) -> R) -> R {
        let p = trans.mem_with(|m| m.range(usize::from(self.base), usize::from(self.u64s)));
        f(unsafe { &mut *(p.as_ptr() as *mut Subbuf<T>) })
    }
}

/// A sub-buffer's entries, in the transaction's memory: Ts, one after
/// another, in the state T says.
#[repr(transparent)]
pub struct Subbuf<T: ?Sized> {
    _entries: PhantomData<T>,
    u64s:     [u64],
}

impl<T: ?Sized> Subbuf<T> {
    /// Its entries, as the u64s they are.
    pub fn u64s(&self) -> &[u64] {
        &self.u64s
    }
}

unsafe impl<T: ?Sized> Container for Subbuf<T> {
    type Entry = T;

    fn entry_bytes(&self) -> &[u8] {
        self.u64s.as_bytes()
    }

    fn entry_bytes_mut(&mut self) -> &mut [u8] {
        self.u64s.as_mut_bytes()
    }
}

/* Validated keys: the thin walk, as a Validated container's keys are walked */
impl BtreeTransSubbuf<BkeyI> {
    pub fn iter<R>(self, t: &TransAttempt<'_, '_>, f: impl FnOnce(ViewEntries<'_, BkeyI>) -> R) -> R {
        self.with(t.trans(), |s| f(ViewEntries::of(s)))
    }

    pub fn iter_mut<R>(self, t: &TransAttempt<'_, '_>, f: impl FnOnce(ViewEntriesMut<'_, BkeyI>) -> R) -> R {
        self.with_mut(t.trans(), |s| f(ViewEntriesMut::of(s)))
    }
}

impl BtreeTransSubbuf<JsetEntry<Validated>> {
    pub fn iter<R>(self, t: &TransAttempt<'_, '_>,
                   f: impl FnOnce(ValidatedEntries<'_, JsetEntry<Validated>>) -> R) -> R {
        self.with(t.trans(), |s| f(ValidatedEntries::of(s)))
    }

    pub fn iter_mut<R>(self, t: &TransAttempt<'_, '_>,
                       f: impl FnOnce(ValidatedEntriesMut<'_, JsetEntry<Validated>>) -> R) -> R {
        self.with_mut(t.trans(), |s| f(ValidatedEntriesMut::of(s)))
    }
}

impl TransAttempt<'_, '_> {
    /// The updates queued, for an error message.
    #[cold]
    pub fn updates_to_text(&self, out: &mut Printbuf) {
        // SAFETY: the transaction's, read
        self.trans().updates_to_text(out)
    }

    /// An entry with header @head at the sub-buffer's end, and @tail_u64s past
    /// it for @tail to write - as long as @head says, which is checked.
    pub fn subbuf_push<T: SelfSized + Unsized + DstHead<Head: Plain> + ?Sized>(&self, cell: &Cell<BtreeTransSubbuf<T>>,
                                                                            head: &T::Head, tail_u64s: usize,
                                                                            tail: impl FnOnce(&mut [u64]))
        -> Result<(), BchError>
    {
        const { assert!(size_of::<T::Head>() == T::TAIL && T::TAIL % size_of::<u64>() == 0 &&
                        size_of::<T::Elem>() == size_of::<u64>()) };

        let head_u64s = T::TAIL / size_of::<u64>();
        let mut p = self.subbuf_alloc(cell, head_u64s + tail_u64s)?;
        let e = unsafe { p.as_mut() };
        let (h, t) = e.split_at_mut(head_u64s);
        h.as_mut_bytes().copy_from_slice(plain_bytes(head));
        tail(t);
        assert!(T::len_in(e.as_bytes()).is_ok_and(|n| n == size_of_val(e)),
                "a sub-buffer entry of {tail_u64s} u64s past its header, which says otherwise");
        Ok(())
    }

    /// The first of the sub-buffer's keys @f takes, for @f to change - and
    /// gone, if @f says Some(true): whether @f took one.
    pub fn subbuf_find_mut(&self, cell: &Cell<BtreeTransSubbuf<BkeyI>>,
                           mut f: impl FnMut(&mut BkeyI) -> Option<bool>)
        -> bool
    {
        let found = cell.get().iter_mut(self, |keys| keys.with_offsets().find_map(|(at, k)| {
            let at = at / size_of::<u64>();
            let range = at..at + usize::from(k.k.u64s);
            f(k).map(|remove| remove.then_some(range))
        }));
        if let Some(Some(range)) = &found {
            self.subbuf_remove(cell, range.clone());
        }
        found.is_some()
    }

    fn subbuf_alloc<T: ?Sized>(&self, cell: &Cell<BtreeTransSubbuf<T>>, u64s: usize)
        -> Result<NonNull<[u64]>, BchError>
    {
        let buf = cell.get();
        if usize::from(buf.u64s) + u64s > usize::from(buf.size) {
            self.subbuf_grow(cell, u64s)?;
        } else {
            cell.set(BtreeTransSubbuf { u64s: buf.u64s + u64s as u16, ..buf });
        }
        let buf = cell.get();
        Ok(self.trans().mem_with(|m| m.range(usize::from(buf.base + buf.u64s) - u64s, u64s)))
    }

    pub fn subbuf_reserve<T: ?Sized>(&self, cell: &Cell<BtreeTransSubbuf<T>>, u64s: usize)
        -> Result<(), BchError>
    {
        let buf = cell.get();
        if usize::from(buf.u64s) + u64s > usize::from(buf.size) {
            self.subbuf_grow(cell, u64s)?;
            cell.set(BtreeTransSubbuf { u64s: buf.u64s, ..cell.get() });
        }
        Ok(())
    }

    fn subbuf_grow<T: ?Sized>(&self, buf: &Cell<BtreeTransSubbuf<T>>, u64s: usize) -> Result<(), BchError> {
        let b = buf.get();
        let new_top = usize::from(b.u64s) + u64s;
        assert!(new_top.next_power_of_two() <= u16::MAX as usize,
                "transaction sub-buffer of {new_top} u64s: past a u16 of them");
        let new_size = if new_top > usize::from(b.size) { new_top.next_power_of_two() } else { usize::from(b.size) };

        let n = self.kmalloc(new_size * size_of::<u64>())?.as_ptr().addr();
        let base = self.trans().mem_with(|m| {
            let base = (n - m.buf.as_ptr().addr()) / size_of::<u64>();
            m.buf.copy_within(usize::from(b.base)..usize::from(b.base + b.u64s), base);
            base
        });
        assert!(base <= u16::MAX as usize, "transaction sub-buffer at u64 {base}: past a u16 of them");

        buf.set(BtreeTransSubbuf { base: base as u16, u64s: new_top as u16, size: new_size as u16, ..b });
        Ok(())
    }

    fn subbuf_remove<T: ?Sized>(&self, cell: &Cell<BtreeTransSubbuf<T>>, range: Range<usize>) {
        let len = range.len();
        cell.get().with_mut(self.trans(), |s| s.u64s.copy_within(range.end.., range.start));
        cell.set(BtreeTransSubbuf { u64s: cell.get().u64s - len as u16, ..cell.get() });
    }

    fn journal_entry_add_bytes(&self, ty: JsetEntryType, btree: u8, level: u8, data: &[u8])
        -> Result<(), BchError>
    {
        let mut h = Head::<JsetEntry>::default();
        h.type_ = ty.0 as u8;
        h.btree_id = btree;
        h.level = level;
        let u64s = data.len().div_ceil(size_of::<u64>());
        h.set_u64s(u64s);

        self.subbuf_push(&self.trans().journal_entries, &h, u64s, |v| {
            let (d, pad) = v.as_mut_bytes().split_at_mut(data.len());
            d.copy_from_slice(data);
            pad.fill(0);
        })
    }

    /// An entry of @ty for @btree at @level, holding @k, with this
    /// transaction's commit.
    pub(crate) fn journal_entry_add<V: ?Sized>(&self, ty: JsetEntryType, btree: u8, level: u8, k: &BkeyI<V>)
        -> Result<(), BchError>
    {
        self.journal_entry_add_bytes(ty, btree, level, k.as_bytes())
    }

    /// @s into the journal, with this transaction's commit.
    pub fn log_str(&self, s: &[u8]) -> Result<(), BchError> {
        self.journal_entry_add_bytes(JsetEntryType::log, 0, 1, s)
    }

    /// @msg into the journal, with this transaction's commit;
    /// ENOMEM_trans_log_msg if building @msg ran out of memory.
    pub fn log_msg(&self, msg: &Printbuf) -> Result<(), BchError> {
        if msg.allocation_failure() {
            return Err(Errcode::ENOMEM_trans_log_msg.into());
        }
        self.log_str(msg.as_bytes())
    }

    /// @k, a key of @btree at @level, into the journal with this
    /// transaction's commit.
    pub fn log_bkey(&self, btree: Btree, level: u32, k: &BkeyI) -> Result<(), BchError> {
        self.journal_entry_add(JsetEntryType::log_bkey, btree.0 as u8, level as u8, k)
    }

    pub fn set_journal_seq_to_pin(&self, seq: u64) {
        self.trans().journal_seq_to_pin.set(seq)
    }
}

impl<'a, 't> TransAttempt<'a, 't> {
    /// Run @f in this attempt's commit, under its btree write locks. It runs
    /// before the commit is certain - what follows can still fail it - and a
    /// restart drops it, with the rest of the attempt's memory.
    ///
    /// That memory is freed without running destructors, so @f is Copy; and
    /// it outlives this call, so 'static.
    pub fn commit_hook<F>(&self, f: F) -> Result<(), BchError>
    where
        F: Fn(&TransAttempt<'_, '_>) -> Result<(), BchError> + Copy + 'static,
    {
        #[repr(C)]
        struct Hook<F> {
            h: BtreeTransCommitHook,
            f: F,
        }

        /// # Safety
        /// @h is the first field of a Hook<F>.
        unsafe fn run<F>(t: &TransAttempt<'_, '_>, h: *mut BtreeTransCommitHook) -> Result<(), BchError>
        where
            F: Fn(&TransAttempt<'_, '_>) -> Result<(), BchError>,
        {
            // SAFETY: @h is the first field of the Hook<F> commit_hook() queued.
            let hook = unsafe { &*(h as *const Hook<F>) };
            (hook.f)(t)
        }

        // bch2_trans_kmalloc() gives 8 byte alignment:
        const { assert!(core::mem::align_of::<Hook<F>>() <= 8) };

        let hooks = &self.trans().hooks;
        let hook = self.kmalloc(size_of::<Hook<F>>())?.as_mut_ptr() as *mut Hook<F>;
        unsafe {
            hook.write(Hook {
                h: BtreeTransCommitHook { fn_: Some(run::<F>), next: hooks.get() },
                f,
            });
            hooks.set(&raw mut (*hook).h);
        }
        Ok(())
    }

    /// The @K at @pos in @btree, as a mutable copy grown to all of its value,
    /// for @edit - then queued as its update, the update's from there.
    /// ENOENT_bkey_type_mismatch if it isn't a @K; nothing queued if @edit
    /// fails. @iter_flags for the lookup - BtreeIterFlags::CACHED, for a btree
    /// read through the key cache - and @flags for the update: C passes both
    /// in one enum.
    pub fn bkey_get_mut<K: BkeyWhole, R>(
        &self,
        btree:      Btree,
        pos:        Bpos,
        iter_flags: BtreeIterFlags,
        flags:      UpdateTriggerFlags,
        edit:       impl FnOnce(&mut TransBkey<'a, 't, K>) -> Result<R, BchError>,
    ) -> Result<R, BchError>
    where
        K::Validated: MutBkey<Mut = K>,
    {
        let mut iter = BtreeIter::new(self, btree, pos, iter_flags | BtreeIterFlags::INTENT);
        let k = self.bkey_make_mut_noupdate(iter.peek_slot_typed::<K>(self)?)?;
        self.edit_update(&iter, k, flags, edit)
    }

    /// @k, @edit's to edit, then queued as its update at @iter: nothing
    /// queued if @edit fails.
    pub fn edit_update<V: ?Sized + ValidatedVal, R>(
        &self,
        iter:  &BtreeIter<'a>,
        mut k: TransBkey<'a, 't, V>,
        flags: UpdateTriggerFlags,
        edit:  impl FnOnce(&mut TransBkey<'a, 't, V>) -> Result<R, BchError>,
    ) -> Result<R, BchError> {
        let r = edit(&mut k)?;
        self.update(iter, k, flags)?;
        Ok(r)
    }

    /// @k, the key at @iter, as bkey_make_mut_noupdate() makes it, for @edit -
    /// then queued as its update, the update's from there. Nothing queued if
    /// @edit fails.
    pub fn bkey_make_mut<V: MutBkey + ?Sized, R>(
        &self,
        iter:  &BtreeIter<'a>,
        k:     BkeySC<'_, V>,
        flags: UpdateTriggerFlags,
        edit:  impl FnOnce(&mut TransBkey<'a, 't, V::Mut>) -> Result<R, BchError>,
    ) -> Result<R, BchError>
    where
        V::Mut: ValidatedVal,
    {
        let k = self.bkey_make_mut_noupdate(k)?;
        self.edit_update(iter, k, flags, edit)
    }

    /// Overwrite @old, the extent at @iter, with @new where they overlap.
    pub fn update_extent_overwrite(
        &self,
        iter:      &BtreeIter<'a>,
        mut flags: UpdateTriggerFlags,
        old:       BkeySC<'_>,
        new:       BkeySC<'_>,
    ) -> Result<(), BchError> {
        let btree = iter.btree();

        /*
         * Split fragments below are fresh kkeys derived from @old, so the
         * caller's BTREE_TRIGGER_set_needs_reconcile_done (asserting "I
         * already set the reconcile field on the kkey I'm inserting") doesn't
         * apply to them — let the trigger compute it.
         */
        flags -= UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE;
        let front_split = bkey_lt(old.k.start_pos(), new.k.start_pos());
        let back_split  = bkey_gt(old.k.p, new.k.p);

        if unlikely(front_split || back_split) {
            self.extent_overwrite_splits(iter, flags, old, new, front_split, back_split)?;
        }

        if !back_split {
            let mut update = self.bkey_alloc(BKEY_U64S as u32)?;
            let k = update.k_mut();
            *k = Bkey::new();
            k.p = old.k.p;
            k.p.snapshot = new.k.p.snapshot;

            if btree.has_snapshots() &&
                (new.k.p.snapshot != old.k.p.snapshot ||
                 self.need_whiteout_for_snapshot(btree, update.k().p)?) {
                update.k_mut().set_key_type(extent_whiteout_type(self.fs(), iter.btree(), new.k));
            }

            self.insert_nonextent(btree, update, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | flags)?;
        }

        Ok(())
    }

    /*
     * @old is being split: re-insert the parts @new doesn't cover - front and back
     * fragments, and the middle when @new is in a different snapshot - with the
     * whiteouts that keep them from showing through where they shouldn't.
     */
    fn extent_overwrite_splits(
        &self,
        iter:        &BtreeIter<'a>,
        flags:       UpdateTriggerFlags,
        old:         BkeySC<'_>,
        new:         BkeySC<'_>,
        front_split: bool,
        back_split:  bool,
    ) -> Result<(), BchError> {
        let fs = self.fs();
        let btree = iter.btree();
        let new_start = new.k.start_pos();
        let middle_split = old.k.p.snapshot != new.k.p.snapshot;
        let nr_splits = u32::from(front_split) + u32::from(back_split) + u32::from(middle_split);

        /*
         * If we're going to be splitting a compressed extent, note it
         * so that __bch2_trans_commit() can increase our disk
         * reservation:
         */
        if nr_splits > 1 {
            let old_d = old.durability_safe(fs);
            let compressed_sectors = old_d.sectors_compressed;

            if compressed_sectors != 0 {
                self.extra_disk_res_add(u64::from(compressed_sectors) * u64::from(nr_splits - 1),
                                        old_d.nr_replicas.into());
            }
        }

        if front_split {
            let mut update = self.bkey_make_mut_noupdate(old)?;

            cut_back_s(new_start, BkeyS::from(&mut update));

            self.insert_snapshot_whiteouts(btree, old.k.p, update.k().p)?;
            self.insert_nonextent(btree, update, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | flags)?;
        }

        /* If we're overwriting in a different snapshot - middle split: */
        if middle_split {
            let mut update = self.bkey_make_mut_noupdate(old)?;

            cut_front_s(fs, new_start, BkeyS::from(&mut update));
            cut_back_s(new.k.p, BkeyS::from(&mut update));

            self.insert_snapshot_whiteouts(btree, old.k.p, update.k().p)?;
            self.insert_nonextent(btree, update, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | flags)?;

            /*
             * A deletion is not inserted below.  When it leaves a back split,
             * preserve the deletion at the new end so the middle fragment in
             * the ancestor is not visible in the snapshot being updated.
             */
            if back_split && new.k.key_type().is_deleted() {
                let mut update = self.bkey_alloc(BKEY_U64S as u32)?;
                let k = update.k_mut();
                *k = Bkey::new();
                k.p = new.k.p;
                k.set_key_type(extent_whiteout_type(fs, btree, new.k));

                self.insert_nonextent(btree, update, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | flags)?;
            }
        }

        if back_split {
            let mut update = self.bkey_make_mut_noupdate(old)?;

            cut_front_s(fs, new.k.p, BkeyS::from(&mut update));

            self.update_by_path(&mut self.trans().paths(), iter.path, QueuedKey::from(&mut update),
                                UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | flags, crate::ret_ip!());
        }

        Ok(())
    }

    fn update_extent(&self, orig_iter: &BtreeIter<'a>, mut insert: TransBkey<'a, 't>,
                     flags: UpdateTriggerFlags) -> Result<(), BchError> {
        let fs = self.fs();
        let btree = orig_iter.btree();
        let end = Bpos { inode: insert.k().p.inode, offset: u64::MAX, snapshot: 0 };

        let mut iter = BtreeIter::new(self, btree, insert.k().start_pos(),
                                      BtreeIterFlags::INTENT | BtreeIterFlags::NOT_EXTENTS |
                                      BtreeIterFlags::NOFILTER_WHITEOUTS);

        'out: {
            let Some((it, k)) = iter.peek_with(self, &mut self.trans().paths(), |i, t, p| i.peek_max_in(t, p, end))?
            else { break 'out };

            let mut advance = false;
            if bkey_eq(k.k.p, insert.k().start_pos()) {
                if bkey_maybe_mergable(k.k, insert.k()) {
                    self.extent_front_merge(it, k, &mut insert, flags)?;
                }

                advance = true;
            }

            loop {
                if advance {
                    iter.advance();
                }
                advance = true;

                let Some((it, k)) = iter.peek_with(self, &mut self.trans().paths(), |i, t, p| i.peek_max_in(t, p, end))?
                else { break 'out };

                assert!(bkey_gt(k.k.p, insert.k().start_pos()),
                        "extent update at {}: overlapping key at {}", insert.k().start_pos(), k.k.p);

                /*
                 * When KEY_TYPE_whiteout is included, bkey_start_pos is not
                 * monotonically increasing
                 */
                if k.k.key_type() != BkeyType::whiteout && bkey_le(insert.k().p, k.k.start_pos()) {
                    break;
                }

                let done = k.k.key_type() != BkeyType::whiteout && bkey_lt(insert.k().p, k.k.p);

                if k.k.key_type().is_extent_whiteout() {
                    let whiteout_type = extent_whiteout_type(fs, btree, insert.k());

                    if bkey_le(k.k.p, insert.k().p) && k.k.key_type() != whiteout_type {
                        let mut update = self.bkey_make_mut_noupdate(k)?;

                        update.k_mut().p.snapshot = it.snapshot();
                        update.k_mut().set_key_type(whiteout_type);

                        self.update(it, update, UpdateTriggerFlags::empty())?;
                    }
                } else {
                    self.update_extent_overwrite(it, flags, k, insert.as_sc())?;
                }

                if done {
                    break 'out;
                }
            }

            let Some((it, k)) = iter.peek_with(self, &mut self.trans().paths(), |i, t, p| i.peek_max_in(t, p, end))?
            else { break 'out };
            if bkey_maybe_mergable(insert.k(), k.k) {
                self.extent_back_merge(it, &mut insert, k)?;
            }
        }

        if !insert.k().key_type().is_deleted() {
            self.insert_nonextent_raw(btree, insert, flags)
        } else {
            Ok(())
        }
    }

    fn extent_front_merge(&self, iter: &BtreeIter<'a>, k: BkeySC<'_>, insert: &mut TransBkey<'a, 't>,
                          flags: UpdateTriggerFlags) -> Result<(), BchError> {
        if unlikely(self.trans().journal_replay_not_finished()) {
            return Ok(());
        }

        let mut update = self.bkey_make_mut_noupdate(k)?;

        if !bkey_merge(self.fs(), BkeyS::from(&mut update), insert.as_sc()) {
            return Ok(());
        }

        if self.key_has_snapshot_overwrites(iter.btree(), k.k.p)? ||
            self.key_has_snapshot_overwrites(iter.btree(), insert.k().p)? {
            return Ok(());
        }

        self.delete_at(iter, flags)?;

        *insert = update;
        Ok(())
    }

    fn extent_back_merge(&self, iter: &BtreeIter<'a>, insert: &mut TransBkey<'a, 't>, k: BkeySC<'_>)
        -> Result<(), BchError>
    {
        if unlikely(self.trans().journal_replay_not_finished()) {
            return Ok(());
        }

        if self.key_has_snapshot_overwrites(iter.btree(), insert.k().p)? ||
            self.key_has_snapshot_overwrites(iter.btree(), k.k.p)? {
            return Ok(());
        }

        bkey_merge(self.fs(), BkeyS::from(insert), k);
        Ok(())
    }

    /// Have the commit reserve @sectors more, at @nr_replicas, for data an
    /// update rewrites.
    pub fn extra_disk_res_add(&self, sectors: u64, nr_replicas: u32) {
        let t = self.trans();
        t.extra_disk_res.set(t.extra_disk_res.get() + sectors);
        t.extra_disk_res_replicas.set(t.extra_disk_res_replicas.get().max(nr_replicas as u8));
    }

    /// Queue @key as an update at @iter's position. Takes the iterator
    /// shared: it doesn't move it - what it sets up there, the key cache
    /// path, is the iterator's own business. @key is the update's from here.
    pub fn update<V: ?Sized + ValidatedVal>(
        &self,
        iter:  &BtreeIter<'a>,
        key:   TransBkey<'a, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        self.update_ip(iter, key.into_any(), flags, crate::ret_ip!())
    }

    /// update(), and the key lent back: edits to it are the update's, till
    /// the commit. Not for extents, whose updates cut their key to fit.
    pub fn update_lent<V: ?Sized + ValidatedVal>(
        &self,
        iter:  &BtreeIter<'a>,
        key:   TransBkey<'a, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<TransBkey<'a, 't, V>, BchError> {
        assert!(!iter.is_extents(), "an extent update can't lend its key, at {}", iter.pos());
        self.update_nonextent_ip(iter, key, flags, crate::ret_ip!())
    }

    fn update_ip(&self, iter: &BtreeIter<'a>, key: TransBkey<'a, 't>,
                 flags: UpdateTriggerFlags, ip: c_ulong) -> Result<(), BchError> {
        if iter.is_extents() {
            return self.update_extent(iter, key, flags);
        }
        self.update_nonextent_ip(iter, key, flags, ip).map(drop)
    }

    fn update_nonextent_ip<V: ?Sized>(&self, iter: &BtreeIter<'a>, mut key: TransBkey<'a, 't, V>,
                                      flags: UpdateTriggerFlags, ip: c_ulong)
        -> Result<TransBkey<'a, 't, V>, BchError>
    {
        if key.k().key_type().is_deleted() &&
            !flags.contains(UpdateTriggerFlags::KEY_CACHE_RECLAIM) &&
            iter.flags & BtreeIterFlags::FILTER_SNAPSHOTS.bits() != 0 &&
            self.need_whiteout_for_snapshot(iter.btree(), key.k().p)? {
            key.k_mut().set_key_type(BkeyType::whiteout);
        }

        let t = self.trans();
        let mut path_idx = if iter.update_path != 0 { iter.update_path } else { iter.path };
        let p = key.k().p;

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!(p == iter.pos(), "update at {}, iterator at {}", p, iter.pos());
            let path_pos = t.paths()[path_idx].pos;
            if p != path_pos {
                assert!(iter.flags & BtreeIterFlags::FILTER_SNAPSHOTS.bits() != 0,
                        "update at {}, path at {}, not filtering snapshots", p, path_pos);
                assert!(bkey_eq(p, path_pos), "update at {}, path at {}", p, path_pos);
                assert!(is_ancestor(self, p.snapshot, path_pos.snapshot),
                        "update at {}, path at {}: not its ancestor", p, path_pos);
            }
        }

        let mut paths = t.paths();

        /*
         * Ensure that updates to cached btrees go to the key cache:
         */
        let path = &paths[path_idx];
        if !flags.contains(UpdateTriggerFlags::KEY_CACHE_RECLAIM) &&
           !path.cached() && path.level() == 0 && btree_id_cached(path.btree_id()) {
            self.update_get_key_cache(&mut paths, iter)?;

            path_idx = iter.key_cache_path.get();
        }

        let i = self.update_by_path(&mut paths, path_idx, QueuedKey::from(&mut key), flags, ip);

        /*
         * If a key is present in the key cache, it must also exist in the
         * btree - this is necessary for cache coherency. When iterating over
         * a btree that's cached in the key cache, the btree iter code checks
         * the key cache - but the key has to exist in the btree for that to
         * work:
         */
        let u = &paths.updates()[i];
        if u.bits.cached() && (u.old_btree_u64s == 0 ||
                               u.key().k.key_type().is_deleted() ||
                               paths[path_idx].locked_key_cached().expect("a cached update's key cache entry")
                                   .immediate_flush()) {
            drop(paths);
            self.flush_new_cached_update(i, flags, ip)?;
        }
        Ok(key)
    }

    /// The update queued: its index in the transaction's updates.
    fn update_by_path(&self, paths: &mut Paths<'_>, path_idx: PathIdx, key: QueuedKey, flags: UpdateTriggerFlags,
                      ip: c_ulong) -> usize
    {
        let fs = self.fs();
        let t = self.trans();

        let path = &paths[path_idx];
        let (btree, level) = (path.btree_id(), u32::from(path.level()));
        let n = BtreeInsertEntry::new(key, path_idx, btree, level, path.cached(), flags.bits(), ip);
        let k_p = n.key().k.p;

        ebug_assert!(path.should_be_locked(), "update by path {path_idx}, which isn't locked");
        ebug_assert!((t.nr_updates.get() as usize) < t.nr_paths.get() as usize,
                     "{} updates queued, and {} paths", t.nr_updates.get(), t.nr_paths.get());
        ebug_assert!(k_p == path.pos, "update at {k_p}, path at {}", path.pos);

        t.set_has_interior_updates(t.has_interior_updates() || path.level() != 0);

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            for w in paths.updates().windows(2) {
                assert!(insert_entry_cmp(&w[0], &w[1]) == Ordering::Less, "the transaction's updates out of order");
            }
        }

        /*
         * Pending updates are kept sorted: first, find position of new update,
         * then delete/trim any updates the new update overwrites:
         */
        let updates = paths.updates();
        let i = updates.iter().position(|u| insert_entry_cmp(&n, u) != Ordering::Greater).unwrap_or(updates.len());
        let overwrite = i < updates.len() && insert_entry_cmp(&n, &updates[i]) == Ordering::Equal;

        if overwrite {
            /*
             * Overwriting an update whose triggers already ran normally
             * loses or duplicates their effects - unless the caller has
             * handled that, e.g. deleting a key from its own trigger, where
             * the new (deleted) key re-runs triggers via the key cache flush.
             */
            let u = paths.update_mut(i);
            ebug_assert!(!(u.bits.insert_trigger_run() || u.bits.overwrite_trigger_run()) ||
                         flags.contains(UpdateTriggerFlags::OVERWRITE_TRIGGERED),
                         "overwriting an update whose triggers ran");

            let old_path = u.path;
            t.path_put(paths, old_path, true);

            let u = paths.update_mut(i);
            u.flags        = n.flags;
            u.bits.set_cached(n.bits.cached());
            u.set_key(n.queued_key());
            u.path         = n.path;
            u.ip_allocated = n.ip_allocated;
        } else {
            let mut old_k = Bkey::new();
            let old_v = peek_slot_exact(&paths[path_idx], &mut old_k);

            let u = paths.insert_update(i, n);
            u.old_k = old_k;
            u.old_v = old_v;
            u.old_btree_u64s = if !old_k.key_type().is_deleted() { old_k.u64s } else { 0 };

            if let Some(jk) = self.journal_keys() {
                if let Some(j_k) = journal_overlay::peek_slot(jk, btree, level, k_p) {
                    u.old_k = j_k.k;
                    u.old_v = (&raw const j_k.v).cast();
                }
            }
        }

        let path = paths.update_mut(i).path;
        paths.get_ll(path, true);

        fs.event_trace(events::update_by_path, |out| {
            prt!(out, "{} overwrite {}\n", t.fn_name(), overwrite as u32);
            t.path_to_text_short(out, paths, path_idx);
            crate::btree::bkey::bkey_val_to_text(out, Some(fs), BkeySC::from(n.key()));
        });

        i
    }

    fn flush_new_cached_update(&self, i: usize, flags: UpdateTriggerFlags, ip: c_ulong) -> Result<(), BchError> {
        let t = self.trans();
        let (btree, pos) = {
            let paths = t.paths();
            let u = &paths.updates()[i];
            (u.bits.btree_id(), u.old_k.p)
        };

        let mut iter = BtreeIter::new(self, btree, pos, BtreeIterFlags::INTENT);

        iter.traverse(self)?;

        /*
         * Taking the iterator and traversing it both take paths, and if that
         * grows the paths table trans->updates moves with it - they're a single
         * allocation, see btree_paths_realloc():
         */
        let mut paths = t.paths();
        t.path_set_should_be_locked(&mut paths, iter.path);

        let u = paths.update_mut(i);
        u.bits.set_key_cache_already_flushed(true);
        u.flags |= UpdateTriggerFlags::NORUN.bits();
        let (key, old_k, old_v) = (u.queued_key(), u.old_k, u.old_v);

        let i = self.update_by_path(&mut paths, iter.path, key, flags, ip);

        let u = paths.update_mut(i);
        u.old_k = old_k;
        u.old_v = old_v;
        u.bits.set_key_cache_flushing(true);
        Ok(())
    }

    fn update_get_key_cache(&self, paths: &mut Paths<'_>, iter: &BtreeIter<'a>) -> Result<(), BchError> {
        let t = self.trans();
        let kcp = iter.key_cache_path.get();

        if kcp == 0 || !paths[kcp].should_be_locked() || paths[kcp].pos != iter.pos() {
            /*
             * Both of these can reallocate trans->paths, so reset
             * key_cache_path every time iter->key_cache_path is set: the
             * index survives, the pointer doesn't.
             */
            let intent = iter.flags & BtreeIterFlags::INTENT.bits() != 0;
            if iter.key_cache_path.get() == 0 {
                let flags = if intent { BtreeIterFlags::INTENT } else { BtreeIterFlags::empty() } | BtreeIterFlags::CACHED;
                iter.key_cache_path.set(t.path_get(paths, iter.btree(), iter.pos(), 1, 0, flags, crate::ret_ip!()));
            }

            iter.key_cache_path.set(t.path_set_pos(paths, iter.key_cache_path.get(), iter.pos(), intent,
                                                   crate::ret_ip!()));

            t.path_traverse(self, paths, iter.key_cache_path.get(), BtreeIterFlags::CACHED)?;

            if paths[iter.key_cache_path.get()].locked_key_cached().expect("a traversed key cache path's entry").dirty() {
                self.fs().event_inc_trace(events::trans_restart_key_cache_raced, |out| prt!(out, "{}", t.fn_name()));
                return Err(self.restart(Errcode::transaction_restart_key_cache_raced));
            }

            t.path_set_should_be_locked(paths, iter.key_cache_path.get());
        }

        Ok(())
    }

    /*
     * When deleting, check if we need to emit a whiteout (because we're overwriting
     * something in an ancestor snapshot)
     */
    fn need_whiteout_for_snapshot(&self, btree: Btree, pos: Bpos) -> Result<bool, BchError> {
        let snapshot = pos.snapshot;

        if self.fs().snapshots.parent(pos.snapshot).is_none_or(|p| p == 0) {
            return Ok(false);
        }

        let pos = Bpos { snapshot: pos.snapshot + 1, ..pos };

        let mut iter = BtreeIter::new(self, btree, pos, BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::NOPRESERVE);
        while let Some(k) = iter.peek(self)? {
            if !bkey_eq(k.k.p, pos) {
                break;
            }

            if is_ancestor(self, snapshot, k.k.p.snapshot) {
                return Ok(!k.k.key_type().is_whiteout());
            }
            iter.advance();
        }

        Ok(false)
    }

    /*
     * Triggers may need to mutate (and possibly grow) the in-flight new key.
     *
     * Fast path: if the existing buffer is already big enough, hand back @op.new
     * unchanged — no trans->updates walk needed.
     *
     * Slow path (growth required): find the queued update by (btree, level, pos),
     * kmalloc a fresh buffer, and rewire i->k / i->k_buf_u64s. We re-find on each
     * call because nested updates can grow trans->updates and invalidate any
     * pointer captured earlier.
     *
     * Mem-trigger phase forbids growth: the journal layout is already committed
     * to. Overwrite-only callers (not inserting) have no business
     * mutating @new (it's the deleted-dummy stub).
     */
    pub(crate) fn update_grow(&self, btree: Btree, level: u32, pos: Bpos, needed_u64s: u32)
        -> Result<*mut BkeyI<Val>, BchError>
    {
        let mut paths = self.trans().paths();
        let i = paths.update_keys()
            .position(|(e, k)| e.bits.btree_id() == btree && u32::from(e.bits.level()) == level && k.k.p == pos)
            .unwrap_or_else(|| panic!("growing an update not queued: {btree} level {level} at {pos}"));

        let mut new = self.bkey_alloc(needed_u64s)?;

        let u = paths.update_mut(i);
        let old = u.key().as_bytes();
        new.as_mut_u64s().as_mut_bytes()[..old.len()].copy_from_slice(old);
        let new = QueuedKey::from(&mut new);
        u.set_key(new);
        Ok(new.as_ptr())
    }

    fn key_has_snapshot_overwrites(&self, btree: Btree, pos: Bpos) -> Result<bool, BchError> {
        if !btree.has_snapshots() || self.fs().snapshots.is_leaf(pos.snapshot) == Ok(true) {
            return Ok(false);
        }

        has_overwrites(self, btree, pos)
    }

    /// A key moved from @old_pos to @new_pos, in the same snapshot: in every
    /// descendant snapshot where it was overwritten at @old_pos, whiteout
    /// @new_pos too, so it stays hidden there.
    pub fn insert_snapshot_whiteouts(&self, btree: Btree, old_pos: Bpos, new_pos: Bpos)
        -> Result<(), BchError>
    {
        assert_eq!(old_pos.snapshot, new_pos.snapshot);

        if !btree.has_snapshots() || bkey_eq(old_pos, new_pos) {
            return Ok(());
        }

        let ids = crate::snapshots::snapshot::overwrites(self, btree, old_pos)?;

        for &id in ids.iter() {
            /*
             * In fsck, a repair can fan whiteouts across an unbounded
             * number of snapshots: commit and restart before the batch
             * hits the trans mem cap. The re-drive converges - committed
             * whiteouts fail the KEY_TYPE_deleted check below and are
             * skipped. Runtime callers (extent splits) are excluded so
             * their whiteouts stay atomic with the extent update:
             */
            if self.fs().flags.get(FsFlag::in_fsck) {
                self.commit_lazy_if_full(CommitFlags::NO_ENOSPC)?;
            }

            let pos = Bpos { snapshot: id, ..new_pos };

            let mut iter = BtreeIter::new(self, btree, pos, BtreeIterFlags::NOT_EXTENTS | BtreeIterFlags::INTENT);
            let Some((it, k)) = iter.peek_with(self, &mut self.trans().paths(), |i, t, p| i.peek_slot_in(t, p))? else {
                panic!("peek_slot() at {pos} in {btree}: no key, not even a deleted one");
            };

            if k.k.key_type().is_deleted() {
                let mut update = self.bkey_alloc(BKEY_U64S as u32)?;
                let k = update.k_mut();
                *k = Bkey::new();
                k.p = pos;
                k.set_key_type(BkeyType::whiteout);

                self.update(it, update, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
            }
        }

        Ok(())
    }

    /// @btree is a bitset btree.
    pub fn bit_mod(&self, btree: Btree, pos: Bpos, set: bool) -> Result<(), BchError> {
        let mut iter = BtreeIter::new(self, btree, pos, BtreeIterFlags::INTENT);
        iter.traverse(self)?;
        self.bit_mod_iter(&iter, set)
    }

    /// Point @iter, intent locked, at the first empty slot in @btree between
    /// @start and @end, after the last key: ENOSPC_btree_slot if there's none.
    pub fn bkey_get_empty_slot(
        &self,
        iter:  &mut BtreeIter<'a>,
        btree: Btree,
        start: Bpos,
        end:   Bpos,
    ) -> Result<(), BchError> {
        *iter = BtreeIter::new(self, btree, end, BtreeIterFlags::INTENT);
        iter.peek_prev(self)?;

        if iter.pos() < start {
            iter.set_pos(start);
        } else {
            iter.advance();
        }

        let k = iter.peek_slot(self)?.expect("peek_slot() returns a key, deleted for a hole");
        assert!(k.k.key_type().is_deleted(), "an empty slot with a key in it");

        if bkey_gt(k.k.p, end) {
            return Err(self.fs().err(Errcode::ENOSPC_btree_slot));
        }

        Ok(())
    }

    /// bit_mod(), through the write buffer.
    pub fn bit_mod_buffered(&self, btree: Btree, pos: Bpos, set: bool) -> Result<(), BchError> {
        if set {
            self.update_buffered_key(btree, BkeyI::<Set>::at(pos).as_ref())
        } else {
            self.update_buffered_key(btree, BkeyI::<Deleted>::at(pos).as_ref())
        }
    }

    /// bit_mod(), at @iter's position: bch2_btree_bit_mod_iter().
    pub fn bit_mod_iter(&self, iter: &BtreeIter<'a>, set: bool) -> Result<(), BchError> {
        let mut k = self.bkey_alloc(BKEY_U64S as u32)?;
        let h = k.k_mut();
        *h = Bkey::new();
        h.set_key_type(if set { BkeyType::set } else { BkeyType::deleted });
        h.p = iter.pos();
        if iter.is_extents() {
            h.resize(1);
        }

        self.update(iter, k, UpdateTriggerFlags::empty())
    }

    /// @key, for write buffered @btree, through the write buffer: as
    /// bch2_trans_update_buffered(). Copied - into the transaction's journal
    /// entries, or before journal replay's done, an update of its own.
    pub fn update_buffered(&self, btree: Btree, key: impl AsRef<BkeyI>) -> Result<(), BchError> {
        self.update_buffered_key(btree, key.as_ref())
    }

    fn update_buffered_key(&self, btree: Btree, k: &BkeyI) -> Result<(), BchError> {
        insert_checks(self.fs(), btree, k)?;

        /*
         * Most updates skip the btree write buffer until journal replay is
         * finished because synchronization with journal replay relies on having
         * a btree node locked - if we're overwriting a key in the journal that
         * journal replay hasn't yet replayed, we have to mark it as
         * overwritten.
         *
         * But accounting updates don't overwrite, they're deltas, and they have
         * to be flushed to the btree strictly in order for journal replay to be
         * able to tell which updates need to be applied:
         */
        if k.k.key_type() != BkeyType::accounting &&
            btree != Btree::need_discard &&
            unlikely(self.trans().journal_replay_not_finished()) {
            let n = self.bkey_copy(k)?;
            return self.insert(btree, n, UpdateTriggerFlags::empty());
        }

        self.journal_entry_add(JsetEntryType::write_buffer_keys, btree.0 as u8, 0, k)
    }

    pub fn insert<V: ?Sized + ValidatedVal>(
        &self,
        btree: impl Into<u32>,
        key:   TransBkey<'a, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        self.insert_with(btree, key, BtreeIterFlags::empty(), flags)
    }

    /// As insert(), with flags for the iterator the insert goes through, too.
    pub fn insert_with<V: ?Sized + ValidatedVal>(
        &self,
        btree:      impl Into<u32>,
        key:        TransBkey<'a, 't, V>,
        iter_flags: BtreeIterFlags,
        flags:      UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        self.insert_raw(Btree(btree.into()), key.into_any(), iter_flags.bits() | flags.bits())
    }

    /// @flags are the iterator's and the update's, one enum, as C has them.
    fn insert_raw(&self, btree: Btree, k: TransBkey<'a, 't>, flags: u32) -> Result<(), BchError> {
        let mut iter = BtreeIter::new(self, btree, k.k().p,
                                      BtreeIterFlags::INTENT | BtreeIterFlags::from_bits_retain(flags));
        iter.traverse(self)?;
        self.update_ip(&iter, k, UpdateTriggerFlags::from_bits_retain(flags), crate::ret_ip!())
    }

    pub fn insert_nonextent<V: ?Sized + ValidatedVal>(
        &self,
        btree: impl Into<u32>,
        key:   TransBkey<'a, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        self.insert_nonextent_raw(Btree(btree.into()), key.into_any(), flags)
    }

    fn insert_nonextent_raw(&self, btree: Btree, k: TransBkey<'a, 't>, flags: UpdateTriggerFlags)
        -> Result<(), BchError>
    {
        let mut iter = BtreeIter::new(self, btree, k.k().p,
                                      BtreeIterFlags::CACHED | BtreeIterFlags::NOT_EXTENTS | BtreeIterFlags::INTENT);
        iter.traverse(self)?;
        self.update_ip(&iter, k, flags, crate::ret_ip!())
    }

    pub fn delete_at(
        &self,
        iter:  &BtreeIter<'a>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let mut k = self.bkey_alloc(BKEY_U64S as u32)?;
        *k.k_mut() = Bkey::new();
        k.k_mut().p = iter.pos();
        self.update(iter, k, flags)
    }

    pub fn delete(
        &self,
        btree: impl Into<u32>,
        pos:   Bpos,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let mut iter = BtreeIter::new(self, btree, pos, BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
        iter.traverse(self)?;
        self.delete_at(&iter, flags)
    }
}

impl BtreeTrans<'_> {
    /// Delete [@start, @end) in @btree, committing as it goes. It drives its
    /// own begin and commit loop, so it's on the transaction, not an attempt:
    /// transaction_restart_nested if it restarted along the way, for a caller
    /// in an attempt to begin again.
    pub fn delete_range(
        &self,
        btree: Btree,
        start: Bpos,
        end:   Bpos,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        self.delete_range_flags(btree, start, end, flags.bits())
    }

    /// delete_range(), @flags the iterator's and the update's, one enum, as C
    /// has them.
    fn delete_range_flags(&self, btree: Btree, start: Bpos, end: Bpos, flags: u32) -> Result<(), BchError> {
        let restart_count = self.restart_count();

        let mut iter = BtreeIter::new(self, btree, start, BtreeIterFlags::INTENT | BtreeIterFlags::from_bits_retain(flags));

        let ret = loop {
            if let Err(e) = lockrestart_do(self, |t| {
                delete_range_one(t, &mut iter, end, UpdateTriggerFlags::from_bits_retain(flags))
            }) {
                break e;
            }
        };

        if !ret.matches(Errcode::delete_range_done) {
            return Err(ret);
        }

        if restart_count != self.restart_count() {
            return Err(BchError::from(Errcode::transaction_restart_nested));
        }
        Ok(())
    }
}

fn delete_range_one<'t>(t: &TransAttempt<'_, 't>, iter: &mut BtreeIter<'t>, end: Bpos, flags: UpdateTriggerFlags)
    -> Result<(), BchError>
{
    let Some((it, k)) = iter.peek_with(t, &mut t.trans().paths(), |i, t, p| i.peek_max_in(t, p, end))? else {
        return Err(Errcode::delete_range_done.into());
    };

    let res = DiskReservation::new(t.fs());

    /*
     * This could probably be more efficient for extents:
     *
     * For extents, iter.pos won't necessarily be the same as
     * bkey_start_pos(k.k) (for non extents they always will be the
     * same). It's important that we delete starting from iter.pos
     * because the range we want to delete could start in the middle
     * of k.
     *
     * (bch2_btree_iter_peek() does guarantee that iter.pos >=
     * bkey_start_pos(k.k)).
     */
    let mut delete = t.bkey_alloc_typed::<Deleted>()?;
    delete.k_mut().p = it.pos();

    if it.is_extents() {
        delete.k_mut().resize((end.min(k.k.p).offset - it.pos().offset) as u32);
    }

    t.update(it, delete, flags)?;
    t.commit(Some(&res), CommitFlags::NO_ENOSPC)
}
