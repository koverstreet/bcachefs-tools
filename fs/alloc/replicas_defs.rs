// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/replicas_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use cstruct_macros::c_enum;

c_enum! {
    #[closed]
    pub enum bch_write_check: u32 {
        /* Starting: is there anywhere to write each data type at all? */
        BCH_WRITE_CHECK_start,
        /*
         * A device leaving the rw set: also refuse, unless the matching force
         * flag is set, if that takes a data type below its configured replicas
         */
        BCH_WRITE_CHECK_dev_leaving_rw,
    }
}
