// SPDX-License-Identifier: GPL-2.0

//! The journal replays the journal read keeps - c->journal_entries - for the
//! commands that print them: list_journal, journal_rewind_info, kvdb's
//! list_journal.
//!
//! The last put of a journal_keys ref frees them: going read-write without
//! retain_recovery_info drops the initial one, and so does the filesystem's
//! exit. rust_collect_journal_entries() takes a ref of its own with the
//! array of them, which JournalReplays holds - with a borrow of the Fs -
//! until it's dropped; so each Replay it hands out is there to read for as
//! long as it lives. Gone already, there are none.

use core::marker::PhantomData;
use core::ops::Deref;

use bch_bindgen::c;
use bcachefs_kernel::fs::Fs;

/// The journal's replays, from the journal read: in seq order, pinned until
/// this is dropped.
pub struct JournalReplays<'a> {
    fs:      &'a Fs,
    entries: c::rust_journal_entries,
}

impl<'a> JournalReplays<'a> {
    /// @fs's replays - none if they've been freed: recovery done, without
    /// retain_recovery_info.
    pub fn collect(fs: &'a Fs) -> Self {
        // SAFETY: an Fs is open
        Self { fs, entries: unsafe { c::rust_collect_journal_entries(fs.as_ptr()) } }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.nr == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = Replay<'_>> + '_ {
        let ptrs: &[*mut c::journal_replay] = if self.entries.entries.is_null() {
            &[]
        } else {
            // SAFETY: the array collect() got, nr long, ours until drop
            unsafe { core::slice::from_raw_parts(self.entries.entries, self.entries.nr) }
        };
        ptrs.iter().map(|&p| Replay { p, _r: PhantomData })
    }
}

impl Drop for JournalReplays<'_> {
    fn drop(&mut self) {
        // SAFETY: the array, and the ref, collect() took
        unsafe { c::rust_journal_entries_put(self.fs.as_ptr(), &mut self.entries) }
    }
}

/// One journal replay: its fields, and its jset.
#[derive(Clone, Copy)]
pub struct Replay<'a> {
    p:  *const c::journal_replay,
    _r: PhantomData<&'a c::journal_replay>,
}

impl<'a> Replay<'a> {
    /// Its jset, as long as its count says: past the struct's end, which a
    /// &journal_replay doesn't reach.
    pub fn jset(&self) -> &'a c::jset {
        // SAFETY: a replay from the journal read, which validated its jset -
        // pinned for 'a
        unsafe { c::journal_replay::jset(self.p) }
    }

    pub fn seq(&self) -> u64 {
        self.j.seq.get()
    }
}

impl Deref for Replay<'_> {
    type Target = c::journal_replay;

    fn deref(&self) -> &c::journal_replay {
        // SAFETY: as jset()'s
        unsafe { &*self.p }
    }
}
