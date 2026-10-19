// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::ffi::c_char;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, c_const, c_enum, c_extern, c_xmacro};
use nestify::nest;

c_xmacro! {
    RECONCILE_SCAN_TYPES(x) {
        (fs),
        (metadata),
        (pending),
        (stripes),
        (device),
        (inum),
    }
}

c_enum! {
    #[closed]
    pub enum reconcile_scan_type: u32 {
        RECONCILE_SCAN_TYPES(t) => RECONCILE_SCAN_ ## t,
    }
}

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]*
    pub struct reconcile_scan {
        pub type_: c::reconcile_scan_type,

        #[c_anon]
        pub id: pub union reconcile_scan_id {
            pub dev: c_uint,
            pub inum: u64,
        },
    }
}
c_default!(reconcile_scan);

c_const! {
    /* No opt change touches more than one bracketed reconcile scan today: */
    #[c_int]
    pub const BCH_OPT_CHANGE_SCANS_MAX: u32 = 4;
}

/*
 * The reconcile-scan cookies an opt change registered as in-flight - see
 * bch2_set_reconcile_needs_scan_pre(). Constructed empty, populated by
 * bch2_opt_hook_pre_set(); the destructor unregisters them, so an opt change
 * that errors out (or never reaches bch2_opt_hook_post_set()) doesn't leak a
 * registration and wedge the reconcile thread on that cookie.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct opt_change_scope {
    pub c: *mut c::bch_fs,
    pub nr: c_uint,
    pub cookies: [u64; c::BCH_OPT_CHANGE_SCANS_MAX as usize],
}
c_default!(opt_change_scope);

c_extern! {
    pub static bch2_reconcile_opts: [*const c_char; 0];
    pub fn bch2_opt_change_scope_exit(scope: *mut c::opt_change_scope);
    pub fn bch2_set_reconcile_needs_scan_trans(trans: *mut c::btree_trans, s: c::reconcile_scan) -> c_int;
    pub fn bch2_set_reconcile_needs_scan(c: *mut c::bch_fs, s: c::reconcile_scan, wakeup: bool) -> c_int;
    pub fn bch2_reconcile_scan_cookie_is_set(trans: *mut c::btree_trans, inum: u64) -> c_int;
    pub fn rust_bch2_reconcile_wakeup(c: *mut c::bch_fs);
    pub fn bch2_reconcile_status_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
    pub fn bch2_reconcile_scan_pending_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
}

impl Fs {
    /// Kick the reconcile thread.
    pub fn reconcile_wakeup(&self) {
        unsafe { rust_bch2_reconcile_wakeup(self.as_ptr()) }
    }

    /// Have reconcile scan for pending work, and kick it: as
    /// bch2_reconcile_pending_wakeup().
    pub fn reconcile_pending_wakeup(&self) -> Result<(), BchError> {
        let scan = c::reconcile_scan {
            type_: c::reconcile_scan_type::RECONCILE_SCAN_pending,
            ..Default::default()
        };
        ret_to_result_void(unsafe { bch2_set_reconcile_needs_scan(self.as_ptr(), scan, true) })
    }

    pub fn reconcile_status_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_reconcile_status_to_text(out.as_raw(), self.as_ptr()) }
    }

    pub fn reconcile_scan_pending_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_reconcile_scan_pending_to_text(out.as_raw(), self.as_ptr()) }
    }
}
