// SPDX-License-Identifier: GPL-2.0

//! Btree updates - btree/update.c's, as they're converted.

use crate::btree::bkey::BkeyI;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::errcode::{bch_errcode, errptr_to_result, ret_to_c, BchError};
use crate::fs::Fs;
use crate::journal::entries::EntryWriter;
use crate::util::ffi::{c_char, Opaque};
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint, CStr};
use core::mem::size_of;
use core::ptr::NonNull;
use core::slice;
use cstruct_macros::c_export;

/// The whiteout that deletes @k in @btree. A KEY_TYPE_extent_whiteout says
/// there's no extent there: key start positions, extent_whiteouts included -
/// KEY_TYPE_whiteouts aren't - only go up. It's a deletion's in a leaf
/// snapshot of an extents btree, once the filesystem has the feature, which
/// this asks for; a snapshot the table doesn't have is a leaf, as C took it.
/// Any other deletion's is KEY_TYPE_whiteout.
pub fn extent_whiteout_type(fs: &Fs, btree: c::btree_id, k: &c::bkey) -> c::bch_bkey_type {
    if btree.is_extents_snapshots() &&
       k.is_deleted() &&
       fs.snapshots.is_leaf(k.p.snapshot) != Ok(false) &&
       fs.request_incompat_feature(c::bcachefs_metadata_version::extent_snapshot_whiteouts).is_ok() {
        c::bch_bkey_type::KEY_TYPE_extent_whiteout
    } else {
        c::bch_bkey_type::KEY_TYPE_whiteout
    }
}

#[c_export]
pub fn bch2_extent_whiteout_type(c: &Opaque<c::bch_fs>, btree: c::btree_id, k: &c::bkey) -> c::bch_bkey_type {
    extent_whiteout_type(Fs::from_c(c), btree, k)
}

// ── Log entries, with a transaction's commit ─────────────────────────────

impl TransAttempt<'_, '_> {
    /// @u64s of journal entries to go in with this transaction's commit, for
    /// one to be written into. The entries are a sub-buffer of transaction
    /// memory, which grows by moving there; this is the attempt's, as the
    /// rest of its memory is.
    fn jset_entry_alloc(&self, u64s: usize) -> Result<&mut [u8], BchError> {
        let trans = self.raw();
        // SAFETY: the transaction's entries, which only its attempt touches
        let p = unsafe {
            let buf = &raw mut (*trans).journal_entries;
            if usize::from((*buf).u64s) + u64s > usize::from((*buf).size) {
                c::__bch2_trans_subbuf_alloc(trans, buf, u64s as c_uint, 0)
            } else {
                let p = (*trans).mem.cast::<u64>().add(usize::from((*buf).base + (*buf).u64s));
                (*buf).u64s += u64s as u16;
                p.cast()
            }
        };
        let p = errptr_to_result(NonNull::new(p).expect("journal entries, or an error"))?;
        // SAFETY: @u64s of transaction memory, ours
        Ok(unsafe { slice::from_raw_parts_mut(p.as_ptr().cast::<u8>(), u64s * size_of::<u64>()) })
    }

    /// An entry of @ty for @btree at @level, holding @data, with this
    /// transaction's commit.
    fn journal_entry_add(&self, ty: c::bch_jset_entry_type, btree: u8, level: u8, data: &[u8])
        -> Result<(), BchError>
    {
        let u64s = c::JSET_KEYS_U64s + data.len().div_ceil(size_of::<u64>());
        EntryWriter::new(self.jset_entry_alloc(u64s)?).push(ty, btree, level, &[data])
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
    pub fn log_bkey(&self, btree: c::btree_id, level: u32, k: &BkeyI) -> Result<(), BchError> {
        // SAFETY: the whole key, as long as it says
        let bytes = unsafe { slice::from_raw_parts(k.as_ffi().cast::<u8>(), k.k.bytes()) };
        self.journal_entry_add(c::bch_jset_entry_type::BCH_JSET_ENTRY_log_bkey, btree.0 as u8, level as u8, bytes)
    }
}

/// # Safety
/// @trans is in an attempt; @str a string.
#[c_export]
pub unsafe fn bch2_trans_log_str(trans: &BtreeTrans<'_>, str: *const c_char) -> c_int {
    // SAFETY: the caller's
    let (t, s) = unsafe { (trans.attempt_in_progress(), CStr::from_ptr(str.cast())) };
    ret_to_c(t.log_str(s.to_bytes()))
}

/// # Safety
/// @trans is in an attempt.
#[c_export]
pub unsafe fn bch2_trans_log_msg(trans: &BtreeTrans<'_>, buf: &mut Printbuf) -> c_int {
    // SAFETY: the caller's
    ret_to_c(unsafe { trans.attempt_in_progress() }.log_msg(buf))
}
