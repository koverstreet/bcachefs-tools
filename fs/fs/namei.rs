// SPDX-License-Identifier: GPL-2.0

use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};

pub fn link_trans(
    trans:    &BtreeTrans,
    dir_inum: c::subvol_inum,
    dir:      &mut c::bch_inode_unpacked,
    inum:     c::subvol_inum,
    inode:    &mut c::bch_inode_unpacked,
    name:     &c::qstr,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_link_trans(trans.raw(), dir_inum, dir, inum, inode, name)
    })
}

pub fn unlink_trans(
    trans:    &BtreeTrans,
    dir_inum: c::subvol_inum,
    dir:      &mut c::bch_inode_unpacked,
    target:   c::subvol_inum,
    inode:    &mut c::bch_inode_unpacked,
    name:     &c::qstr,
    deleting: bool,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_unlink_trans(trans.raw(), dir_inum, dir, target, inode, name, deleting)
    })
}

#[allow(clippy::too_many_arguments)]
pub fn create_trans(
    trans:        &BtreeTrans,
    dir_inum:     c::subvol_inum,
    dir:          &mut c::bch_inode_unpacked,
    inode:        &mut c::bch_inode_unpacked,
    subvol:       &mut c::bch_subvolume,
    name:         &c::qstr,
    uid:          c::uid_t,
    gid:          c::gid_t,
    mode:         c::umode_t,
    rdev:         c::dev_t,
    snapshot_src: c::subvol_inum,
    flags:        u32,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_create_trans(
            trans.raw(),
            dir_inum,
            dir,
            inode,
            subvol,
            name,
            uid,
            gid,
            mode,
            rdev,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            snapshot_src,
            flags,
        )
    })
}
