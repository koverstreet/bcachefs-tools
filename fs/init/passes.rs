// SPDX-License-Identifier: GPL-2.0

//! Recovery passes (init/passes.h). The pass table and the code that runs
//! it are C; a pass converted to Rust is a function the table calls by name.

/// Export @pass, a `fn(&Fs) -> Result<(), E>` with E converting to BchError,
/// as the recovery pass the C pass table calls @name:
///
/// `recovery_pass!(bch2_check_dirents => check_dirents);`
#[macro_export]
macro_rules! recovery_pass {
    ($name:ident => $pass:path) => {
        /// # Safety
        /// @c is a live filesystem: the pass table's caller.
        #[no_mangle]
        pub unsafe extern "C" fn $name(c: *mut $crate::c::bch_fs) -> core::ffi::c_int {
            let fs = unsafe { $crate::fs::Fs::borrow_raw(c) };

            match $pass(&fs) {
                Ok(())  => 0,
                Err(e)  => -$crate::errcode::BchError::from(e).raw(),
            }
        }
    };
}

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::Printbuf;

/// Schedule recovery pass @pass, saying so to @out: as
/// bch2_run_explicit_recovery_pass(). An error - a restart of recovery - only
/// if it has to go back to an earlier pass.
pub fn run_explicit(
    fs:    &Fs,
    out:   &mut Printbuf,
    pass:  c::bch_recovery_pass,
    flags: c::bch_run_recovery_pass_flags,
) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_run_explicit_recovery_pass(fs.raw, out.as_raw(), pass, flags) })
}

/// Have recovery pass @pass run before going on, if it hasn't this mount: as
/// bch2_require_recovery_pass().
pub fn require(fs: &Fs, out: &mut Printbuf, pass: c::bch_recovery_pass) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::bch2_require_recovery_pass(fs.raw, out.as_raw(), pass) })
}

/// Whether btree @btree has been checked consistent and not changed since: as
/// bch2_btree_is_clean().
pub fn btree_is_clean(fs: &Fs, btree: c::btree_id) -> bool {
    unsafe { c::bch2_btree_is_clean(fs.raw, btree) }
}

/// Record btree @btree as checked consistent: as bch2_set_btree_clean().
pub fn set_btree_clean(fs: &Fs, btree: c::btree_id) {
    unsafe { c::bch2_set_btree_clean(fs.raw, btree) }
}
