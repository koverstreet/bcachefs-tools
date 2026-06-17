// SPDX-License-Identifier: GPL-2.0

use crate::btree::iter::{TransAttempt, TransError};
use crate::c;

pub fn link_trans<'a, 't>(
    t:        TransAttempt<'a, 't>,
    dir_inum: c::subvol_inum,
    dir:      &mut c::bch_inode_unpacked,
    inum:     c::subvol_inum,
    inode:    &mut c::bch_inode_unpacked,
    name:     &c::qstr,
) -> Result<TransAttempt<'a, 't>, TransError<'a, 't>> {
    let ret = unsafe {
        c::bch2_link_trans(t.raw(), dir_inum, dir, inum, inode, name)
    };
    t.result(ret)
}

pub fn unlink_trans<'a, 't>(
    t:        TransAttempt<'a, 't>,
    dir_inum: c::subvol_inum,
    dir:      &mut c::bch_inode_unpacked,
    target:   c::subvol_inum,
    inode:    &mut c::bch_inode_unpacked,
    name:     &c::qstr,
    deleting: bool,
) -> Result<TransAttempt<'a, 't>, TransError<'a, 't>> {
    let ret = unsafe {
        c::bch2_unlink_trans(t.raw(), dir_inum, dir, target, inode, name, deleting)
    };
    t.result(ret)
}

#[allow(clippy::too_many_arguments)]
pub fn create_trans<'a, 't>(
    t:            TransAttempt<'a, 't>,
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
) -> Result<TransAttempt<'a, 't>, TransError<'a, 't>> {
    let ret = unsafe {
        c::bch2_create_trans(
            t.raw(),
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
    };
    t.result(ret)
}
