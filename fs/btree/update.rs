// SPDX-License-Identifier: GPL-2.0

//! Btree updates - btree/update.c's, as they're converted.

#![allow(non_camel_case_types)]

use crate::btree::bkey::{bkey_eq, BkeyI, BkeySC, BkeyV, BkeyWhole, MutBkey};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{bch_errcode, errptr_to_result, ret_to_c, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::journal::entries::EntryWriter;
use crate::util::ffi::Opaque;
use crate::util::{Printbuf, ToText};
use core::cell::Cell;
use core::ffi::{c_int, c_uint, c_ulong};
use core::mem::size_of;
use core::ops::Range;
use core::ptr::NonNull;
use core::slice;
use cstruct_macros::{c_export, c_enum, c_extern};
use crate::format::{Bpos, Btree, BkeyType, MetadataVersion};
use zerocopy::IntoBytes;

c_enum! {
    #[closed]
    #[prefix(__BCH_TRANS_COMMIT_)]
    #[mask(bch_trans_commit_flags: u32, prefix = BCH_TRANS_COMMIT_)]
    #[c_list(BCH_TRANS_COMMIT_FLAGS)]
    pub enum __bch_trans_commit_flags: u32 {
        /* First bits for bch_watermark: */
        /// don't check for enospc
        no_enospc = c::BCH_WATERMARK_BITS + 1,
        /// don't attempt to take a ref on c->writes
        no_check_rw,
        /// don't take a journal reservation, instead pin journal entry
        /// referred to by trans->journal_res.seq
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
}

c_extern! {
    #[c_printf(2, 3)]
    pub fn bch2_fs_log_msg(c: *mut Fs, fmt: *const core::ffi::c_char, ...) -> c_int;
    #[c_printf(2, 3)]
    pub fn bch2_journal_log_msg(c: *mut Fs, fmt: *const core::ffi::c_char, ...) -> c_int;
    pub fn __bch2_trans_subbuf_alloc(trans: *mut BtreeTrans<'_>, buf: *mut c::btree_trans_subbuf, u64s: c_uint,
                                     ip: core::ffi::c_ulong) -> *mut core::ffi::c_void;
    pub fn rust_bch2_trans_update_buffered(trans: *mut BtreeTrans<'_>, btree: Btree, k: *mut c::bkey_i) -> c_int;
    pub fn bch2_trigger_get_mutable_new(trans: *mut BtreeTrans<'_>, op: c::btree_trigger_op, needed_u64s: c_uint,
                                        out: *mut c::bkey_s) -> c_int;
    pub fn bch2_btree_delete_at(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter,
                                flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete(trans: *mut BtreeTrans<'_>, btree: Btree, pos: Bpos,
                             flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert_nonextent(trans: *mut BtreeTrans<'_>, btree: Btree,
                                       k: *mut c::bkey_i, k_buf_u64s: c_uint,
                                       flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert_trans(trans: *mut BtreeTrans<'_>, btree: Btree,
                                   k: *mut c::bkey_i,
                                   flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_insert(c: *mut Fs, id: Btree, k: *mut c::bkey_i,
                             disk_res: *mut c::disk_reservation,
                             commit_flags: c::bch_trans_commit_flags,
                             iter_flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete_range_trans(trans: *mut BtreeTrans<'_>, btree: Btree,
                                         start: Bpos, end: Bpos,
                                         flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_delete_range(c: *mut Fs, id: Btree, start: Bpos, end: Bpos,
                                   flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn bch2_btree_bit_mod(trans: *mut BtreeTrans<'_>, btree: Btree, pos: Bpos,
                              set: bool) -> c_int;
    pub fn bch2_btree_bit_mod_buffered(trans: *mut BtreeTrans<'_>, btree: Btree, pos: Bpos,
                                       set: bool) -> c_int;
    pub fn bch2_btree_bit_mod_iter(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter, set: bool) -> c_int;
    pub fn __bch2_insert_snapshot_whiteouts(trans: *mut BtreeTrans<'_>, btree: Btree,
                                            pos: Bpos, s: *mut c::snapshot_id_list) -> c_int;
    #[must_use]
    pub fn bch2_trans_update_ip(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter,
                                k: *mut c::bkey_i, k_buf_u64s: c_uint,
                                flags: c::btree_iter_update_trigger_flags, ip: c_ulong) -> c_int;
    pub fn bch2_trans_update_extent_overwrite(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter,
                                              flags: c::btree_iter_update_trigger_flags,
                                              old: c::bkey_s_c, new: c::bkey_s_c) -> c_int;
    pub fn bch2_bkey_get_empty_slot(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter,
                                    btree: Btree, start: Bpos, end: Bpos) -> c_int;
    pub fn bch2_trans_commit_hook(trans: *mut BtreeTrans<'_>, h: *mut c::btree_trans_commit_hook);
    pub fn __bch2_trans_commit(trans: *mut BtreeTrans<'_>, flags: c::bch_trans_commit_flags,
                               lazy: bool) -> c_int;
}

/// The whiteout that deletes @k in @btree. A KEY_TYPE_extent_whiteout says
/// there's no extent there: key start positions, extent_whiteouts included -
/// KEY_TYPE_whiteouts aren't - only go up. It's a deletion's in a leaf
/// snapshot of an extents btree, once the filesystem has the feature, which
/// this asks for; a snapshot the table doesn't have is a leaf, as C took it.
/// Any other deletion's is KEY_TYPE_whiteout.
pub fn extent_whiteout_type(fs: &Fs, btree: Btree, k: &c::bkey) -> BkeyType {
    if btree.is_extents_snapshots() &&
       k.key_type().is_deleted() &&
       fs.snapshots.is_leaf(k.p.snapshot) != Ok(false) &&
       fs.request_incompat_feature(MetadataVersion::extent_snapshot_whiteouts).is_ok() {
        BkeyType::extent_whiteout
    } else {
        BkeyType::whiteout
    }
}

#[c_export]
pub fn bch2_extent_whiteout_type(c: &Opaque<Fs>, btree: Btree, k: &c::bkey) -> BkeyType {
    extent_whiteout_type(Fs::from_c(c), btree, k)
}

impl Fs {
    /*
     * Use for logging messages during recovery to enable reserved space and avoid
     * blocking.
     */
    pub fn journal_log_msg(&self, msg: &dyn ToText) -> Result<(), BchError> {
        let mut buf = Printbuf::new();
        msg.to_text(&mut buf);
        ret_to_result_void(unsafe { bch2_journal_log_msg(self.as_ptr(), c"%s".as_ptr().cast(), buf.as_c_str().as_ptr()) })
    }
}

// ── A transaction's updates, and log entries with its commit ────────────

#[derive(Clone, Copy)]
pub enum Subbuf {
    JournalEntries,
    Accounting,
}

impl TransAttempt<'_, '_> {
    /// The updates queued, for an error message.
    #[cold]
    pub fn updates_to_text(&self, out: &mut Printbuf) {
        // SAFETY: the transaction's, read
        self.trans().updates_to_text(out)
    }

    fn subbuf_cell(&self, which: Subbuf) -> &Cell<c::btree_trans_subbuf> {
        match which {
            Subbuf::JournalEntries => &self.trans().journal_entries,
            Subbuf::Accounting     => &self.trans().accounting,
        }
    }

    pub unsafe fn subbuf(&self, which: Subbuf) -> &mut [u64] {
        let buf = self.subbuf_cell(which).get();
        unsafe { self.trans().mem_with(|m| m.range(usize::from(buf.base), usize::from(buf.u64s))).as_mut() }
    }

    pub fn subbuf_alloc(&self, which: Subbuf, u64s: usize) -> Result<&mut [u64], BchError> {
        let cell = self.subbuf_cell(which);
        let buf = cell.get();
        let p = if usize::from(buf.u64s) + u64s > usize::from(buf.size) {
            unsafe { c::__bch2_trans_subbuf_alloc(self.raw(), cell.as_ptr(), u64s as c_uint, 0) }
        } else {
            let p = self.trans().mem_with(|m| m.range(usize::from(buf.base + buf.u64s), u64s));
            cell.set(c::btree_trans_subbuf { u64s: buf.u64s + u64s as u16, ..buf });
            p.as_ptr().cast()
        };
        let p = errptr_to_result(NonNull::new(p).expect("a sub-buffer's u64s, or an error"))?;
        Ok(unsafe { slice::from_raw_parts_mut(p.as_ptr().cast::<u64>(), u64s) })
    }

    pub fn subbuf_remove(&self, which: Subbuf, range: Range<usize>) {
        let len = range.len();
        unsafe { self.subbuf(which) }.copy_within(range.end.., range.start);
        let cell = self.subbuf_cell(which);
        cell.set(c::btree_trans_subbuf { u64s: cell.get().u64s - len as u16, ..cell.get() });
    }

    /// An entry of @ty for @btree at @level, holding @data, with this
    /// transaction's commit.
    fn journal_entry_add(&self, ty: c::bch_jset_entry_type, btree: u8, level: u8, data: &[u8])
        -> Result<(), BchError>
    {
        let u64s = c::JSET_KEYS_U64s + data.len().div_ceil(size_of::<u64>());
        EntryWriter::new(self.subbuf_alloc(Subbuf::JournalEntries, u64s)?.as_mut_bytes())
            .push(ty, btree, level, &[data])
            .expect("an entry the size allocated for it");
        Ok(())
    }

    /// @s into the journal, with this transaction's commit.
    pub fn log_str(&self, s: &[u8]) -> Result<(), BchError> {
        self.journal_entry_add(c::bch_jset_entry_type::log, 0, 1, s)
    }

    /// @msg into the journal, with this transaction's commit;
    /// ENOMEM_trans_log_msg if building @msg ran out of memory.
    pub fn log_msg(&self, msg: &Printbuf) -> Result<(), BchError> {
        if msg.allocation_failure() {
            return Err(bch_errcode::BCH_ERR_ENOMEM_trans_log_msg.into());
        }
        self.log_str(msg.as_bytes())
    }

    /// @k, a key of @btree at @level, into the journal with this
    /// transaction's commit.
    pub fn log_bkey(&self, btree: Btree, level: u32, k: &BkeyI) -> Result<(), BchError> {
        self.journal_entry_add(c::bch_jset_entry_type::log_bkey, btree.0 as u8, level as u8,
                               k.as_bytes())
    }

    pub fn set_journal_seq_to_pin(&self, seq: u64) {
        self.trans().journal_seq_to_pin.set(seq)
    }
}

/// # Safety
/// @trans is in an attempt.
#[c_export]
pub unsafe fn bch2_trans_log_msg(trans: &BtreeTrans<'_>, buf: &mut Printbuf) -> c_int {
    // SAFETY: the caller's
    ret_to_c(unsafe { trans.attempt_in_progress() }.log_msg(buf))
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
            h: c::btree_trans_commit_hook,
            f: F,
        }

        unsafe extern "C" fn run<F>(
            trans: *mut BtreeTrans<'_>,
            h:     *mut c::btree_trans_commit_hook,
        ) -> core::ffi::c_int
        where
            F: Fn(&TransAttempt<'_, '_>) -> Result<(), BchError>,
        {
            // SAFETY: @h is the first field of the Hook<F> commit_hook() queued.
            let hook = unsafe { &*(h as *const Hook<F>) };
            let trans = unsafe { BtreeTrans::borrow_raw(trans) };
            // SAFETY: C runs hooks in the commit, in the attempt being committed
            let t = unsafe { trans.attempt_in_progress() };
            crate::errcode::ret_to_c((hook.f)(&t))
        }

        // bch2_trans_kmalloc() gives 8 byte alignment:
        const { assert!(core::mem::align_of::<Hook<F>>() <= 8) };

        let hook = self.kmalloc(size_of::<Hook<F>>())?.as_mut_ptr() as *mut Hook<F>;
        unsafe {
            hook.write(Hook {
                h: c::btree_trans_commit_hook { fn_: Some(run::<F>), next: core::ptr::null_mut() },
                f,
            });
            c::bch2_trans_commit_hook(self.raw(), &raw mut (*hook).h);
        }
        Ok(())
    }

    /// The @K at @pos in @btree, as a mutable copy already queued as its
    /// update, grown to all of its value - ENOENT_bkey_type_mismatch if it
    /// isn't a @K. @iter_flags for the lookup - BtreeIterFlags::CACHED, for a
    /// btree read through the key cache - and @flags for the update: C passes
    /// both in one enum.
    pub fn bkey_get_mut<K: BkeyWhole>(
        &self,
        btree:      Btree,
        pos:        Bpos,
        iter_flags: BtreeIterFlags,
        flags:      UpdateTriggerFlags,
    ) -> Result<TransBkey<'a, 't, K>, BchError> {
        let mut iter = BtreeIter::new(self, btree, pos, iter_flags | BtreeIterFlags::INTENT);
        let k = iter.peek_slot_typed::<K>(self)?.untyped();
        let k = self.make_mut_noupdate(k, size_of::<BkeyI<K>>())?;
        self.update(&iter, &k, flags)?;
        // SAFETY: a @K's, grown to all of it
        Ok(unsafe { k.whole_unchecked() })
    }

    /// @k, the key at @iter, as bkey_make_mut_noupdate() makes it, already
    /// queued as its update: edits to it go in with the commit.
    pub fn bkey_make_mut<V: MutBkey + ?Sized>(
        &self,
        iter:  &BtreeIter<'a>,
        k:     BkeySC<'_, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<TransBkey<'a, 't, V::Mut>, BchError> {
        let k = self.bkey_make_mut_noupdate(k)?;
        self.update(iter, &k, flags)?;
        Ok(k)
    }

    /// Overwrite @old, the extent at @iter, with @new where they overlap.
    pub fn update_extent_overwrite(
        &self,
        iter:  &BtreeIter<'a>,
        flags: UpdateTriggerFlags,
        old:   BkeySC<'_>,
        new:   BkeySC<'_>,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_trans_update_extent_overwrite(self.raw(), iter.raw(),
                                                  c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
                                                  old.into(), new.into())
        };
        self.result(ret)
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
    /// path, is the iterator's own business. @key is borrowed too: it's in
    /// transaction memory, where the update points, and the caller can go on
    /// reading it - where it went, say.
    pub fn update<V: ?Sized>(
        &self,
        iter:  &BtreeIter<'a>,
        key:   &TransBkey<'_, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        // No caller ip: C's is the call site's, _THIS_IP_, which Rust has
        // no way to name.
        let ret = unsafe {
            c::bch2_trans_update_ip(
                self.raw(),
                iter.raw(),
                key.as_ptr(),
                key.buf_u64s,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
                0,
            )
        };
        self.result(ret)
    }

    /// update(), with a key that isn't in transaction memory - a C caller's,
    /// as bch2_trans_update() takes. Mutably: the update may rewrite it, a
    /// deletion that needs a whiteout becoming one.
    ///
    /// # Safety
    /// @key stays where it is, unchanged, until the commit - or until the
    /// attempt is over, if it doesn't commit. The update points at it.
    pub unsafe fn update_key(
        &self,
        iter:  &BtreeIter<'a>,
        key:   &mut BkeyI,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_trans_update_ip(
                self.raw(),
                iter.raw(),
                key.as_ffi_mut(),
                key.k.u64s as u32,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
                0,
            )
        };
        self.result(ret)
    }

    /// A key moved from @old_pos to @new_pos, in the same snapshot: in every
    /// descendant snapshot where it was overwritten at @old_pos, whiteout
    /// @new_pos too, so it stays hidden there.
    pub fn insert_snapshot_whiteouts(&self, btree: Btree, old_pos: Bpos, new_pos: Bpos)
        -> Result<(), BchError>
    {
        assert_eq!(old_pos.snapshot, new_pos.snapshot);

        if crate::BTREE_HAS_SNAPSHOTS_MASK & (1 << btree.0) == 0 || bkey_eq(old_pos, new_pos) {
            return Ok(());
        }

        let mut ids = crate::snapshots::snapshot::overwrites(self, btree, old_pos)?;
        if ids.is_empty() {
            return Ok(());
        }

        // Lent to C, which only reads it:
        let mut list = c::snapshot_id_list {
            nr:           ids.len(),
            size:         ids.len(),
            data:         ids.as_mut_ptr(),
            preallocated: [],
        };
        let ret = unsafe { c::__bch2_insert_snapshot_whiteouts(self.raw(), btree, new_pos, &mut list) };
        self.result(ret)
    }

    /// @btree is a bitset btree.
    pub fn bit_mod(&self, btree: Btree, pos: Bpos, set: bool) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_bit_mod(self.raw(), btree, pos, set) };
        self.result(ret)
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
        let ret = unsafe { c::bch2_bkey_get_empty_slot(self.raw(), iter.raw(), btree, start, end) };
        self.result(ret)
    }

    /// bit_mod(), through the write buffer.
    pub fn bit_mod_buffered(&self, btree: Btree, pos: Bpos, set: bool) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_bit_mod_buffered(self.raw(), btree, pos, set) };
        self.result(ret)
    }

    /// bit_mod(), at @iter's position: bch2_btree_bit_mod_iter().
    pub fn bit_mod_iter(&self, iter: &BtreeIter<'a>, set: bool) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_bit_mod_iter(self.raw(), iter.raw(), set) };
        self.result(ret)
    }

    /// @key, for write buffered @btree, through the write buffer: as
    /// bch2_trans_update_buffered(). Copied - into the transaction's journal
    /// entries, or before journal replay's done, an update of its own.
    pub fn update_buffered<V: ?Sized>(&self, btree: Btree, key: &TransBkey<'_, 't, V>)
        -> Result<(), BchError>
    {
        // SAFETY: a key, which is only read
        self.result(unsafe { c::rust_bch2_trans_update_buffered(self.raw(), btree, key.as_ptr()) })
    }

    pub fn insert<V: ?Sized>(
        &self,
        btree: impl Into<u32>,
        key:   TransBkey<'_, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        self.insert_with(btree, key, BtreeIterFlags::empty(), flags)
    }

    /// As insert(), with flags for the iterator the insert goes through, too.
    pub fn insert_with<V: ?Sized>(
        &self,
        btree:      impl Into<u32>,
        key:        TransBkey<'_, 't, V>,
        iter_flags: BtreeIterFlags,
        flags:      UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_insert_trans(
                self.raw(),
                Btree::from_raw(btree.into()).expect("invalid btree id"),
                key.as_ptr(),
                c::btree_iter_update_trigger_flags::from_bits_retain(iter_flags.bits() | flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn insert_nonextent<V: ?Sized>(
        &self,
        btree: impl Into<u32>,
        key:   TransBkey<'_, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_insert_nonextent(
                self.raw(),
                Btree::from_raw(btree.into()).expect("invalid btree id"),
                key.as_ptr(),
                key.k().u64s as u32,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn delete_at(
        &self,
        iter:  &BtreeIter<'a>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_delete_at(
                self.raw(),
                iter.raw(),
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn delete(
        &self,
        btree: impl Into<u32>,
        pos:   Bpos,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_delete(
                self.raw(),
                Btree::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            )
        };
        self.result(ret)
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
        crate::errcode::ret_to_result(unsafe {
            c::bch2_btree_delete_range_trans(self.raw(), btree, start, end,
                                             c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()))
        }).map(|_| ())
    }
}
