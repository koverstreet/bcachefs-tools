// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of alloc/background.h - C gets it as prototypes, in
//! alloc/background_gen.h, which is generated from this file: see
//! fs/types/lib.rs.

#![allow(non_camel_case_types)]

use cstruct_macros::c_const;

c_const! {
    /// How far a bucket's generation may get past its oldest pointer's before
    /// it has to be gc'd: the gen is a u8, compared wrapping.
    pub const BUCKET_GC_GEN_MAX: u32 = 96;
}
