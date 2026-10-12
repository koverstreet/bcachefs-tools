use crate::c;
use crate::data::checksum::BCH_NONCE_JOURNAL;
use crate::types::Le32;
use crate::util::vstructs::View;

#[allow(non_camel_case_types)]
pub type journal_entry_type = c::bch_jset_entry_type;

// ---- jset helpers ----

impl c::jset {
    /// The nonce of this journal entry: its seq, in the journal domain.
    pub fn nonce(&self) -> c::nonce {
        let seq = self.seq.get();
        c::nonce {
            d: [
                Le32::new(0),
                Le32::new(seq as u32),
                Le32::new((seq >> 32) as u32),
                Le32::new(BCH_NONCE_JOURNAL),
            ],
        }
    }
}

impl c::journal_replay {
    /// @p's jset, and the bytes its header says it has: a replay is
    /// allocated to hold them, past the struct's end - which a
    /// &journal_replay doesn't reach.
    ///
    /// # Safety
    /// @p is a replay from the journal read, which validated its jset, live
    /// for 'a.
    pub unsafe fn jset<'a>(p: *const Self) -> View<'a, c::jset> {
        // SAFETY: the caller's
        unsafe { View::from_validated(&raw const (*p).j) }
    }
}

// ---- entry type conversion ----

/// Convert entry type byte to the raw journal entry type.
pub fn entry_type(entry: &c::jset_entry) -> c::bch_jset_entry_type {
    c::bch_jset_entry_type(entry.type_ as u32)
}

pub fn entry_type_is_known(t: c::bch_jset_entry_type) -> bool {
    t.0 < journal_entry_type::nr.0
}

/// Convert entry btree_id byte to the enum, if it's a known btree.
pub fn entry_btree_id(entry: &c::jset_entry) -> Option<c::btree_id> {
    c::btree_id::from_raw(entry.btree_id as u32)
}

// ---- jset_entry_log helpers ----

/// A log entry's message, its NUL padding trimmed - empty for any other entry.
pub fn entry_log_msg<'a>(entry: View<'a, c::jset_entry>) -> &'a [u8] {
    let Ok(c::JsetEntryRef::log(l)) = entry.typed() else { return &[] };
    let data = l.as_bytes().get(core::mem::offset_of!(c::jset_entry_log, d)..).unwrap_or_default();
    // Trim trailing nulls
    let len = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..len]
}

pub fn entry_log_str_eq(entry: View<'_, c::jset_entry>, s: &str) -> bool {
    let msg = entry_log_msg(entry);
    msg.len() >= s.len() && &msg[..s.len()] == s.as_bytes()
}
