// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{CacheAligned, c_default};
use crate::util::{Printbuf, ToText};
use cstruct_macros::{c_const, c_extern, CStruct};

c_const! {
    #[c_int]
    pub const BUCKET_NOCOW_LOCKS_BITS: u32 = 10;
    pub const BUCKET_NOCOW_LOCKS: u32 = 1 << c::BUCKET_NOCOW_LOCKS_BITS;

    #[c_int]
    pub const NOCOW_LOCK_BUCKET_SIZE: u32 = 6;
}

#[repr(C)]
#[derive(CStruct)]
pub struct nocow_lock_bucket {
    pub __align: CacheAligned,
    pub wait: c::closure_waitlist,
    pub lock: c::spinlock_t,
    pub b: [u64; c::NOCOW_LOCK_BUCKET_SIZE as usize],
    pub l: [c::atomic_t; c::NOCOW_LOCK_BUCKET_SIZE as usize],
}
c_default!(nocow_lock_bucket);

#[repr(C)]
#[derive(CStruct)]
pub struct bucket_nocow_lock_table {
    pub l: [c::nocow_lock_bucket; c::BUCKET_NOCOW_LOCKS as usize],
}
c_default!(bucket_nocow_lock_table);

c_extern! {
    pub fn bch2_nocow_locks_to_text(out: *mut c::printbuf, t: *mut c::bucket_nocow_lock_table);
}

impl ToText for bucket_nocow_lock_table {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_nocow_locks_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}
