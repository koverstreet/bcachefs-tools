// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::vstructs::Head;
use crate::util::{Printbuf, ToText};
use core::ffi::c_uint;
use cstruct_macros::{CStruct, TypeInfo, c_extern};

/* unsized - bch_replicas_entry_v1 is variable length */
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_replicas_entry_cpu {
    pub ref_: c::atomic_t,
    pub e: Head<c::bch_replicas_entry_v1>,
}
c_default!(bch_replicas_entry_cpu);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_replicas_cpu {
    pub nr: c_uint,
    pub entry_size: c_uint,
    pub entries: *mut c::bch_replicas_entry_cpu,
}
c_default!(bch_replicas_cpu);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub union bch_replicas_padded {
    #[c("u8 bytes[struct_size_t(struct bch_replicas_entry_v1, devs, BCH_BKEY_PTRS_MAX)]")]
    pub bytes: [u8; core::mem::size_of::<Head<c::bch_replicas_entry_v1>>()
                    + c::BCH_BKEY_PTRS_MAX as usize * core::mem::size_of::<u8>()],
    pub e: Head<c::bch_replicas_entry_v1>,
}
c_default!(bch_replicas_padded);

c_extern! {
    pub fn bch2_cpu_replicas_to_text(out: *mut c::printbuf, r: *mut c::bch_replicas_cpu);
    pub fn bch2_dev_has_data(c: *mut c::bch_fs, ca: *mut c::bch_dev) -> c_uint;
}

impl ToText for bch_replicas_cpu {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_cpu_replicas_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}
