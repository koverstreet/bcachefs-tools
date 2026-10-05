// SPDX-License-Identifier: GPL-2.0

//! Subvolumes (snapshots/subvolume.h).

use crate::btree::bkey::BkeySC;
use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};

/// Subvolume @subvol: as bch2_subvolume_get(). With
/// @inconsistent_if_not_found, a missing subvolume is reported as
/// filesystem inconsistency, not just returned as ENOENT.
pub fn get(
    trans:                     &BtreeTrans<'_>,
    subvol:                    u32,
    inconsistent_if_not_found: bool,
) -> Result<c::bch_subvolume, BchError> {
    let mut s = c::bch_subvolume::default();
    ret_to_result_void(unsafe {
        c::bch2_subvolume_get(trans.raw(), subvol, inconsistent_if_not_found, &mut s)
    })?;
    Ok(s)
}

/// Subvolume @subvol's key, for when the key itself is wanted - to print:
/// as bch2_subvolume_get_key().
pub fn get_key(
    trans:                     &BtreeTrans<'_>,
    subvol:                    u32,
    inconsistent_if_not_found: bool,
) -> Result<c::bkey_i_subvolume, BchError> {
    let mut k = c::bkey_i_subvolume::default();
    ret_to_result_void(unsafe {
        c::bch2_subvolume_get_key(trans.raw(), subvol, inconsistent_if_not_found, &mut k)
    })?;
    Ok(k)
}

/// Subvolume @subvol's snapshot: as bch2_subvolume_get_snapshot(). Unlike
/// get(), a deleted-state subvolume is still found.
pub fn get_snapshot(trans: &BtreeTrans<'_>, subvol: u32) -> Result<u32, BchError> {
    let mut snapshot = 0;
    ret_to_result_void(unsafe { c::bch2_subvolume_get_snapshot(trans.raw(), subvol, &mut snapshot) })?;
    Ok(snapshot)
}

/// Whether subvolume @subvol is in state unlinked - the only state in which
/// its root may legitimately be an unlinked directory: as
/// bch2_subvolume_is_unlinked().
pub fn is_unlinked(trans: &BtreeTrans<'_>, subvol: u32) -> Result<bool, BchError> {
    Ok(crate::errcode::ret_to_result(unsafe { c::bch2_subvolume_is_unlinked(trans.raw(), subvol) })? != 0)
}

/// @k's value if it's a subvolume, zero padded past the fields an older key
/// lacks: as bkey_val_copy_pad() on a bkey_s_c_subvolume.
pub fn val(k: BkeySC<'_>) -> Option<c::bch_subvolume> {
    (k.k.type_ == c::bch_bkey_type::KEY_TYPE_subvolume.0 as u8)
        .then(|| unsafe { k.val_copy_pad() })
}

impl c::bch_subvolume {
    pub fn snapshot(&self) -> u32 { u32::from_le(self.snapshot) }

    /// The state field, if it holds a state - not if it's 0, predating the
    /// field, or damaged: as bch2_subvolume_state() and
    /// bch2_subvolume_state_valid(). Read raw, never taken from C as a
    /// bch_subvolume_state: a damaged field is no variant of the Rust enum.
    pub fn state_field(&self) -> Option<c::bch_subvolume_state> {
        use c::bch_subvolume_state::*;

        [SUBVOLUME_STATE_live, SUBVOLUME_STATE_unlinked, SUBVOLUME_STATE_deleted]
            .into_iter()
            .find(|&s| s as u32 == u32::from_le(self.state))
    }

    /// The subvolume's state - read from its flags if it predates the state
    /// field: as bch2_subvolume_state_compat(). None if the field is damaged.
    pub fn state(&self) -> Option<c::bch_subvolume_state> {
        if self.state == 0 {
            Some(unsafe { c::bch2_subvolume_state_from_flags(self) })
        } else {
            self.state_field()
        }
    }

    /// Set the subvolume's state: as bch2_subvolume_state_set().
    pub fn set_state(&mut self, state: c::bch_subvolume_state) {
        unsafe { c::bch2_subvolume_state_set(self, state) }
    }
}
