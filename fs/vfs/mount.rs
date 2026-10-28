// SPDX-License-Identifier: GPL-2.0

//! A mount, as a file opened through it has it: write access to it. (Its
//! idmap is idmap.rs's.)

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::file::File;

/// Write access to a file's mount for as long as this lives: the mount can't
/// go read-only meanwhile.
pub(crate) struct WriteAccess<'a>(&'a File);

impl<'a> WriteAccess<'a> {
    pub(crate) fn get(file: &'a File) -> Result<Self, BchError> {
        ret_to_result_void(unsafe { c::mnt_want_write_file(file.as_ptr()) })?;
        Ok(WriteAccess(file))
    }
}

impl Drop for WriteAccess<'_> {
    fn drop(&mut self) {
        unsafe { c::mnt_drop_write_file(self.0.as_ptr()) }
    }
}
