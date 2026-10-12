use crate::c;
use crate::data::checksum::BCH_NONCE_JOURNAL;
use crate::types::Le32;

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

/// JSET_NO_FLUSH bitfield: bit 5 of le32 flags.
pub fn jset_no_flush(jset: &c::jset) -> bool {
    (jset.flags.get() >> 5) & 1 != 0
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

/// Get log message bytes from a jset_entry of type log.
/// Layout: jset_entry header (8 bytes) followed by d[] message bytes.
pub fn entry_log_msg(entry: &c::jset_entry) -> &[u8] {
    let msg_bytes = entry.u64s.get() as usize * 8;
    if msg_bytes == 0 {
        return &[];
    }
    let ptr = entry as *const c::jset_entry as *const u8;
    let data = unsafe { core::slice::from_raw_parts(ptr.add(8), msg_bytes) };
    // Trim trailing nulls
    let len = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..len]
}

pub fn entry_log_str_eq(entry: &c::jset_entry, s: &str) -> bool {
    let msg = entry_log_msg(entry);
    msg.len() >= s.len() && &msg[..s.len()] == s.as_bytes()
}
