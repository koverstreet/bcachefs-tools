// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::types::c_default;
use crate::util::ffi::c_char;
use crate::util::Printbuf;
use core::ffi::c_uint;
use cstruct_macros::{CStruct, TypeInfo, c_const, c_enum, c_extern, c_typedef, c_xmacro, tagged_union};

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

c_typedef! {
    pub type reconcile_scan_device = c_uint;
}

c_typedef! {
    pub type reconcile_scan_inum = u64;
}

tagged_union! {
    #[derive(Clone, Copy, CStruct)]*
    /// A scan reconcile's asked for - of what, and for a device's or an
    /// inode's, which.
    #[derive(TypeInfo)]
    pub struct reconcile_scan {
        tag type_: enum c::reconcile_scan_type,
        arms from RECONCILE_SCAN_TYPES(x) => x: reconcile_scan_ ## x = c::reconcile_scan_type::RECONCILE_SCAN_ ## x,
        pad: [u64; 2],
        size: pad,
        plain,

        #[repr(C)]
        pub struct reconcile_scan_fs {},
        #[repr(C)]
        pub struct reconcile_scan_metadata {},
        #[repr(C)]
        pub struct reconcile_scan_pending {},
        #[repr(C)]
        pub struct reconcile_scan_stripes {},
    }
}

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
        self.request_scan(c::reconcile_scan::from_arm(c::reconcile_scan_pending {}), true)
    }

    pub fn reconcile_status_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_reconcile_status_to_text(out.as_raw(), self.as_ptr()) }
    }

    pub fn reconcile_scan_pending_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_reconcile_scan_pending_to_text(out.as_raw(), self.as_ptr()) }
    }
}
