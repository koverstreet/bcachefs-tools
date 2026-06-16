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
