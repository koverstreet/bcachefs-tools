// SPDX-License-Identifier: GPL-2.0

//! A journal buf's failed preflushes, for journal_write_done() to log: which
//! devices, and their errors.

use crate::c;
use crate::fs::Fs;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_export;

#[c_export]
pub fn bch2_journal_buf_flush_error(w: &mut c::journal_buf, dev: c_uint, err: c_int) {
    w.flush_failed.set_errcode(dev, err);
}

#[c_export]
pub fn bch2_journal_buf_flush_failed(w: &c::journal_buf) -> bool {
    !w.flush_failed.is_empty()
}

#[c_export]
pub fn bch2_journal_buf_flush_all_dev_removed(w: &c::journal_buf) -> bool {
    w.flush_failed.all_dev_removed()
}

#[cold]
#[c_export]
pub fn bch2_journal_buf_flush_failures_to_text(out: &mut Printbuf, c: &Opaque<Fs>, w: &c::journal_buf) {
    w.flush_failed.to_text(out, Fs::from_c(c));
}
