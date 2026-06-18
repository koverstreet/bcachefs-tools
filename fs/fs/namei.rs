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
) -> Result<TransAttempt<'a, 't>, TransError> {
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
) -> Result<TransAttempt<'a, 't>, TransError> {
    let ret = unsafe {
        c::bch2_unlink_trans(t.raw(), dir_inum, dir, target, inode, name, deleting)
    };
    t.result(ret)
}

pub fn rename_trans<'a, 't>(
    t:           TransAttempt<'a, 't>,
    src_dir:     c::subvol_inum,
    src_dir_u:   &mut c::bch_inode_unpacked,
    dst_dir:     c::subvol_inum,
    dst_dir_u:   &mut c::bch_inode_unpacked,
    src_inode_u: &mut c::bch_inode_unpacked,
    dst_inode_u: &mut c::bch_inode_unpacked,
    src_name:    &c::qstr,
    dst_name:    &c::qstr,
    mode:        c::bch_rename_mode,
) -> Result<TransAttempt<'a, 't>, TransError> {
    let ret = unsafe {
        c::bch2_rename_trans(
            t.raw(),
            src_dir,
            src_dir_u,
            dst_dir,
            dst_dir_u,
            src_inode_u,
            dst_inode_u,
            src_name,
            dst_name,
            mode,
        )
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
) -> Result<TransAttempt<'a, 't>, TransError> {
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
