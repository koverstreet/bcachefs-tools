// SPDX-License-Identifier: GPL-2.0

//! A seq_file being written: what a show() op - show_options(),
//! show_devname() - prints to. The kernel crate's SeqFile prints through its
//! own fmt adapter; this writes bytes, which every Display of ours can make.

use crate::c;
use crate::util::ffi::Opaque;

#[repr(transparent)]
pub struct SeqFile(Opaque<c::seq_file>);

impl SeqFile {
    /// # Safety
    /// @m is a seq_file being written by this thread, for 'a.
    pub unsafe fn from_raw<'a>(m: *mut c::seq_file) -> &'a SeqFile {
        unsafe { &*m.cast::<SeqFile>() }
    }

    /// Past the buffer's end, it's dropped: seq_file retries the show() with
    /// a bigger one.
    pub fn write(&self, b: &[u8]) {
        unsafe { c::seq_write(self.0.as_ptr(), b.as_ptr().cast(), b.len()) };
    }
}
