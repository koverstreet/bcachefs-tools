// SPDX-License-Identifier: GPL-2.0

//! A file's own ioctls that need only the filesystem. vfs/ioctl.rs's
//! dispatch answers them here, with what each needs of the file - its inode
//! number, whether it's a directory - and keeps the VFS's checks of
//! ownership and permission, which come in as arguments or a callback.

use crate::bch_warn;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::ioctl::UserArg;

/// FS_IOC_GETFSLABEL's: the label, without its NUL - and if it fills its
/// field, without its last byte, as C had it.
pub fn get_label(fs: &Fs, user: UserArg<[u8; c::FSLABEL_MAX as usize]>) -> Result<(), BchError> {
    const LABEL_SIZE: usize = c::BCH_SB_LABEL_SIZE as usize;
    const { assert!(LABEL_SIZE < c::FSLABEL_MAX as usize) };

    let label = fs.sb_lock().sb().label;
    let mut len = label.iter().position(|&b| b == 0).unwrap_or(LABEL_SIZE);
    if len == LABEL_SIZE {
        len -= 1;
        bch_warn!(fs, "label is too long, return the first {len} bytes");
    }
    user.bytes_at(0, len).writer().write_slice(&label[..len])?;
    Ok(())
}
