// SPDX-License-Identifier: GPL-2.0

//! The journal replays the journal read keeps - the journal keys' entries -
//! for the commands that print them: list_journal, journal_rewind_info,
//! kvdb's list_journal.
//!
//! JournalReplays is a ref on the journal keys, whose last put frees them: so
//! each Replay it hands out is there to read for as long as it lives. Gone
//! already - recovery done, without retain_recovery_info - there are none.

use core::marker::PhantomData;
use core::ops::Deref;

use bch_bindgen::c;
use bcachefs_kernel::btree::journal_overlay::JournalKeysRef;
use bcachefs_kernel::fs::Fs;

/// The journal's replays, from the journal read: in seq order, pinned until
/// this is dropped. The Fs is borrowed for as long: the ref doesn't keep it,
/// and its teardown doesn't wait for refs.
pub struct JournalReplays<'a> {
    keys: Option<JournalKeysRef>,
    _fs:  PhantomData<&'a Fs>,
}

impl<'a> JournalReplays<'a> {
    pub fn collect(fs: &'a Fs) -> Self {
        Self { keys: fs.journal_keys_get(), _fs: PhantomData }
    }

    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    pub fn iter(&self) -> impl Iterator<Item = Replay<'_>> + '_ {
        self.keys.iter().flat_map(|jk| jk.value().all_replays()).map(|(r, j)| Replay { r, j })
    }
}

/// One journal replay: its fields, and its jset.
#[derive(Clone, Copy)]
pub struct Replay<'a> {
    r: &'a c::journal_replay,
    j: &'a c::jset,
}

impl<'a> Replay<'a> {
    pub fn jset(&self) -> &'a c::jset {
        self.j
    }

    pub fn seq(&self) -> u64 {
        self.j.seq.get()
    }
}

impl Deref for Replay<'_> {
    type Target = c::journal_replay;

    fn deref(&self) -> &c::journal_replay {
        self.r
    }
}
