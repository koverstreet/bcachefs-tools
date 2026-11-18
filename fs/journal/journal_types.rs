// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::{Printbuf, ToText};
use core::ffi::c_uint;
use core::sync::atomic::Ordering::{Relaxed, SeqCst};
use cstruct_macros::bch_enum;

#[bch_enum(flags)]
#[repr(u32)]
pub enum JournalCycleFlags {
    must_close = 1 << 0,
    must_open = 1 << 1,
    force_close = 1 << 2,
}

/* First bits for BCH_WATERMARK: */
#[bch_enum(closed)]
#[repr(u32)]
pub enum JournalResFlag {
    __JOURNAL_RES_GET_NONBLOCK = c::BCH_WATERMARK_BITS,
    __JOURNAL_RES_GET_CHECK,
}

pub const JOURNAL_RES_GET_NONBLOCK: u32 = 1 << c::JournalResFlag::__JOURNAL_RES_GET_NONBLOCK as u32;
pub const JOURNAL_RES_GET_CHECK: u32 = 1 << c::JournalResFlag::__JOURNAL_RES_GET_CHECK as u32;

impl c::journal {
    /// For C, which writes what it changes through it as its locks allow.
    pub fn as_ptr(&self) -> *mut c::journal {
        core::ptr::from_ref(self).cast_mut()
    }

    pub fn halted(&self) -> bool {
        self.reservations().cur_entry_offset() == c::JOURNAL_ENTRY_ERROR_VAL
    }

    /// journal_shutdown if it's halted: bch2_journal_error().
    pub fn error(&self) -> Result<(), BchError> {
        if self.halted() {
            Err(crate::errcode::Errcode::journal_shutdown.into())
        } else {
            Ok(())
        }
    }
}

/* idx follows cur_entry_offset's 22 bits, the buf counts idx's 2 */
const STATE_IDX_SHIFT: u32 = c::JOURNAL_ENTRY_OFFSET_MAX.count_ones();

impl c::journal_res_state {
    pub fn from_v(v: u64) -> Self {
        c::journal_res_state { v }
    }

    pub fn v(self) -> u64 {
        self.v
    }

    fn count_shift(idx: u32) -> u32 {
        c::JOURNAL_STATE_BUF0_SHIFT + idx * c::JOURNAL_STATE_BUF_COUNT_BITS
    }

    pub fn count(self, idx: u32) -> u32 {
        (self.v() >> Self::count_shift(idx)) as u32 & c::JOURNAL_STATE_BUF_COUNT_MAX
    }

    /// Where the open entry's next reservation goes, in u64s - or one of the
    /// JOURNAL_ENTRY_*_VALs, for no entry open.
    pub fn cur_entry_offset(self) -> u32 {
        (self.v() & u64::from(c::JOURNAL_ENTRY_OFFSET_MAX)) as u32
    }

    pub fn set_cur_entry_offset(&mut self, offset: u32) {
        let mask = u64::from(c::JOURNAL_ENTRY_OFFSET_MAX);
        *self = Self::from_v(self.v() & !mask | u64::from(offset) & mask);
    }

    /// The ring slot of the entry open, or last open.
    pub fn idx(self) -> u32 {
        (self.v() >> STATE_IDX_SHIFT) as u32 & c::JOURNAL_STATE_BUF_MASK
    }

    /// @idx, wrapped to a ring slot, as C's 2 bit field wraps.
    pub fn set_idx(&mut self, idx: u32) {
        let mask = u64::from(c::JOURNAL_STATE_BUF_MASK) << STATE_IDX_SHIFT;
        *self = Self::from_v(self.v() & !mask | u64::from(idx) << STATE_IDX_SHIFT & mask);
    }

    pub fn is_open(self) -> bool {
        self.cur_entry_offset() < c::JOURNAL_ENTRY_CLOSED_VAL
    }

    /// A ref on idx()'s buf - false, and none, if its count is full.
    pub fn inc(&mut self) -> bool {
        let idx = self.idx();
        if self.count(idx) == c::JOURNAL_STATE_BUF_COUNT_MAX {
            return false;
        }
        *self = Self::from_v(self.v() + (1 << Self::count_shift(idx)));
        true
    }
}

const _: () = assert!(STATE_IDX_SHIFT + c::JOURNAL_STATE_BUF_MASK.count_ones() == c::JOURNAL_STATE_BUF0_SHIFT,
                      "journal_res_state: idx between cur_entry_offset and the counts");

impl c::journal {
    /// j->reservations, as it is now.
    #[inline]
    pub fn reservations(&self) -> c::journal_res_state {
        c::journal_res_state::from_v(self.lock.hot.reservations.load(Relaxed))
    }

    /// @f of the state swapped in, retried until it sticks: Ok(the state before
    /// it), or Err(the state @f gave None for).
    #[inline]
    pub fn reservations_update(&self, mut f: impl FnMut(c::journal_res_state) -> Option<c::journal_res_state>)
        -> Result<c::journal_res_state, c::journal_res_state>
    {
        let mut old = self.reservations();
        loop {
            let Some(new) = f(old) else {
                return Err(old);
            };
            match self.lock.hot.reservations.compare_exchange(old.v(), new.v(), SeqCst, Relaxed) {
                Ok(_) => return Ok(old),
                Err(v) => old = c::journal_res_state::from_v(v),
            }
        }
    }

    /// A ref put on ring slot @idx's buf: the state after.
    #[inline]
    pub fn state_buf_put(&self, idx: u32) -> c::journal_res_state {
        let d = 1u64 << c::journal_res_state::count_shift(idx);
        c::journal_res_state::from_v(self.lock.hot.reservations.fetch_sub(d, SeqCst) - d)
    }

    /// @seq's buf's reservation count in @s, 0 for a seq too old to have one.
    pub fn state_seq_count(&self, s: c::journal_res_state, seq: u64) -> u32 {
        if self.fs().journal_cur_seq().wrapping_sub(seq) < u64::from(c::JOURNAL_STATE_BUF_NR) {
            s.count((seq & u64::from(c::JOURNAL_STATE_BUF_MASK)) as u32)
        } else {
            0
        }
    }
}

impl ToText for c::journal {
    fn to_text(&self, out: &mut Printbuf) {
        self.debug_to_text(out)
    }
}

impl Fs {
    /// Write a journal entry with nothing in it but metadata, and wait for
    /// it to be flushed.
    pub fn journal_meta(&self) -> Result<(), BchError> {
        self.journal.meta()
    }

    /// Ask the journal not to flush seqs [@start, @end), whose entries hold
    /// nothing that needs it - false if it can't: one's been flushed.
    pub fn journal_noflush_seq(&self, start: u64, end: u64) -> bool {
        self.journal.noflush_seq(start, end)
    }

    /// What every journal entry reserves for @res, made @u64s.
    pub fn journal_entry_res_resize(&self, res: &c::journal_entry_res, u64s: c_uint) {
        self.journal.entry_res_resize(res, u64s)
    }

    /// Advance the rewind limit to @seq: buckets emptied before it may be
    /// discarded, once a journal flush has the limit on disk.
    pub fn journal_advance_rewind_seq(&self, seq: u64) {
        self.journal.advance_rewind_seq(seq)
    }

    /// Start writes of whatever's ready to be written.
    pub fn journal_do_writes(&self) {
        self.journal.do_writes()
    }

    pub fn journal_reclaim_to_text(&self, out: &mut Printbuf) {
        self.journal.reclaim_to_text(out)
    }

    /* journal_replicas, or metadata_replicas when it isn't set: */
    pub fn journal_replicas(&self) -> u32 {
        let opts = self.opts();
        match opts.journal_replicas.get() {
            0 => opts.metadata_replicas.get().into(),
            r => r.into(),
        }
    }
}

