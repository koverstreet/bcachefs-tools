// SPDX-License-Identifier: GPL-2.0

//! Btree updates - btree/update.c's, as they're converted.

#![allow(non_camel_case_types)]

use crate::btree::bkey::BkeyI;
use crate::btree::iter::{BtreeTrans, TransAttempt};
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
use cstruct_macros::{c_export, c_enum, c_extern, c_xmacro};
use crate::format::{Bpos, Btree, BkeyType};
use zerocopy::IntoBytes;

c_xmacro! {
    BCH_TRANS_COMMIT_FLAGS(x) {
        (no_enospc, "don't check for enospc"),
        (no_check_rw, "don't attempt to take a ref on c->writes"),
        (no_journal_res,
         "don't take a journal reservation, instead "
         "pin journal entry referred to by trans->journal_res.seq"),
        (no_skip_noops, "don't drop noop updates"),
        (journal_reclaim,
         "operation required for journal reclaim; may return error "
         "instead of deadlocking if BCH_WATERMARK_reclaim not specified"),
        (journal_replay, "in journal replay"),
        (skip_accounting_apply, "we're in journal replay - accounting updates have already been applied"),
    }
}

c_enum! {
    #[closed]
    pub enum __bch_trans_commit_flags: u32 {
        /* First bits for bch_watermark: */
        __BCH_TRANS_COMMIT_FLAGS_START = c::BCH_WATERMARK_BITS,
        BCH_TRANS_COMMIT_FLAGS(n, ..) => concat(__BCH_TRANS_COMMIT_, n),
    }
}

c_enum! {
    #[flags]
    pub enum bch_trans_commit_flags: u32 {
        BCH_TRANS_COMMIT_FLAGS(n, ..) => concat(BCH_TRANS_COMMIT_, n) = 1 << (c::concat(__BCH_TRANS_COMMIT_, n) as u32),
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
       fs.request_incompat_feature(c::bcachefs_metadata_version::extent_snapshot_whiteouts).is_ok() {
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
        unsafe { c::bch2_trans_updates_to_text(out.as_raw(), self.raw()) }
    }

    fn subbuf_cell(&self, which: Subbuf) -> &Cell<c::btree_trans_subbuf> {
        match which {
            Subbuf::JournalEntries => &self.trans().journal_entries,
            Subbuf::Accounting     => &self.trans().accounting,
        }
    }

    pub unsafe fn subbuf(&self, which: Subbuf) -> &mut [u64] {
        let buf = self.subbuf_cell(which).get();
        unsafe {
            let base = self.trans().mem.get().cast::<u64>().add(usize::from(buf.base));
            slice::from_raw_parts_mut(base, usize::from(buf.u64s))
        }
    }

    pub fn subbuf_alloc(&self, which: Subbuf, u64s: usize) -> Result<&mut [u64], BchError> {
        let cell = self.subbuf_cell(which);
        let buf = cell.get();
        let p = if usize::from(buf.u64s) + u64s > usize::from(buf.size) {
            unsafe { c::__bch2_trans_subbuf_alloc(self.raw(), cell.as_ptr(), u64s as c_uint, 0) }
        } else {
            let p = unsafe { self.trans().mem.get().cast::<u64>().add(usize::from(buf.base + buf.u64s)) };
            cell.set(c::btree_trans_subbuf { u64s: buf.u64s + u64s as u16, ..buf });
            p.cast()
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
        self.journal_entry_add(c::bch_jset_entry_type::BCH_JSET_ENTRY_log, 0, 1, s)
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
        self.journal_entry_add(c::bch_jset_entry_type::BCH_JSET_ENTRY_log_bkey, btree.0 as u8, level as u8,
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
