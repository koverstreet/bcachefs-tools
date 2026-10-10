// SPDX-License-Identifier: GPL-2.0

//! Subvolumes (snapshots/subvolume.h).

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{is_restart, BtreeIter, BtreeIterFlags, BtreeTrans, LoopControl};
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

/// As get_snapshot(), for a caller that checks for a missing subvolume itself:
/// not found isn't recorded as subvol_missing, and doesn't schedule
/// check_inodes - as __bch2_subvolume_get_snapshot(..., warn = false).
pub fn get_snapshot_nowarn(trans: &BtreeTrans<'_>, subvol: u32) -> Result<u32, BchError> {
    let mut snapshot = 0;
    ret_to_result_void(unsafe {
        c::__bch2_subvolume_get_snapshot(trans.raw(), subvol, &mut snapshot, false)
    })?;
    Ok(snapshot)
}

/// EROFS if subvolume @subvol is read-only - a snapshot; otherwise its
/// snapshot: as bch2_subvol_is_ro_trans().
pub fn is_ro_trans(trans: &BtreeTrans<'_>, subvol: u32) -> Result<u32, BchError> {
    let mut snapshot = 0;
    ret_to_result_void(unsafe { c::bch2_subvol_is_ro_trans(trans.raw(), subvol, &mut snapshot) })?;
    Ok(snapshot)
}

/// A new subvolume, as create() made it: its ID, its snapshot, and the
/// subvolume itself.
pub struct Created {
    pub subvol:   u32,
    pub snapshot: u32,
    pub v:        c::bch_subvolume,
}

/// Create a subvolume whose root is inode @inode, in the filesystem tree
/// under subvolume @parent - a snapshot of subvolume @src, unless @src is 0;
/// read-only if @ro: as bch2_subvolume_create().
pub fn create(
    trans:  &BtreeTrans<'_>,
    inode:  u64,
    parent: u32,
    src:    u32,
    ro:     bool,
) -> Result<Created, BchError> {
    let mut new = Created { subvol: 0, snapshot: 0, v: Default::default() };
    ret_to_result_void(unsafe {
        c::bch2_subvolume_create(trans.raw(), inode, parent, src,
                                 &mut new.subvol, &mut new.snapshot, &mut new.v, ro)
    })?;
    Ok(new)
}

/// Unlink subvolume @subvol: its state goes to unlinked, and when the
/// transaction commits its deletion is queued, for once the pagecache is done
/// with it - check_subvols finds one a crash left behind. As
/// bch2_subvolume_unlink().
pub fn unlink(trans: &BtreeTrans<'_>, subvol: u32) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_subvolume_unlink(trans.raw(), subvol) })
}

/// ENOTEMPTY_subvol_not_empty if subvolume @subvol has subvolumes under it:
/// as bch2_subvol_has_children().
pub fn require_no_children(trans: &BtreeTrans<'_>, subvol: u32) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_subvol_has_children(trans.raw(), subvol) })
}

/// Walk @iter to @end as subvolume @subvol sees it - at its snapshot, read
/// again after every restart - calling @f on each key, until it says stop: as
/// for_each_btree_key_in_subvolume_max_continue_in_trans().
///
/// In the caller's transaction, and @f may unlock it: each peek relocks. A
/// restart begins the transaction again and retries the key it happened on,
/// so @f must not restart after doing something it can't redo - which is
/// why a callback that has unlocked returns, and leaves the relock to the
/// next peek.
pub fn for_each_in_subvolume_max_in_trans<'t, F, R>(
    trans:  &BtreeTrans<'t>,
    iter:   &mut BtreeIter<'t>,
    end:    c::bpos,
    subvol: u32,
    flags:  BtreeIterFlags,
    mut f:  F,
) -> Result<R::Break, BchError>
where
    F: FnMut(BkeySC<'_>) -> Result<R, BchError>,
    R: LoopControl,
{
    let mut restart_count = trans.restart_count();
    let mut snapshot = 0;

    loop {
        let ret = (|| -> Result<Option<R::Break>, BchError> {
            trans.relock()?;
            if snapshot == 0 {
                snapshot = get_snapshot(trans, subvol)?;
                iter.set_snapshot(snapshot);
            }
            // This loop is its own driver - it begins again on a restart,
            // below - so this attempt is the one it's in
            let t = trans.attempt_in_progress();
            Ok(match iter.peek_max_type(&t, end, flags)? {
                Some(k) => f(k)?.into_break(),
                None    => Some(Default::default()),
            })
        })();

        match ret {
            Ok(Some(b)) => return Ok(b),
            Ok(None)    => {
                trans.verify_not_restarted(restart_count);
                if !iter.advance() {
                    return Ok(Default::default());
                }
            }
            Err(e) if is_restart(&e) => {
                restart_count = trans.begin_raw();
                snapshot = 0;
            }
            Err(e) => return Err(e),
        }
    }
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

        [c::bch_subvolume_state::SUBVOLUME_STATE_live, c::bch_subvolume_state::SUBVOLUME_STATE_unlinked, c::bch_subvolume_state::SUBVOLUME_STATE_deleted]
            .into_iter()
            .find(|&s| s.0 as u32 == u32::from_le(self.state))
    }

    /// The subvolume's state - read from its flags if it predates the state
    /// field: as bch2_subvolume_state_compat(). None if the field is damaged.
    pub fn state(&self) -> Option<c::bch_subvolume_state> {
        if self.state == 0 {
            // bch2_subvolume_state_from_flags()
            Some(if self.unlinked_obsolete() {
                c::bch_subvolume_state::SUBVOLUME_STATE_unlinked
            } else {
                c::bch_subvolume_state::SUBVOLUME_STATE_live
            })
        } else {
            self.state_field()
        }
    }

    /// Set the subvolume's state: as bch2_subvolume_state_set().
    pub fn set_state(&mut self, state: c::bch_subvolume_state) {
        unsafe { c::bch2_subvolume_state_set(self, state) }
    }
}
