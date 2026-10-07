// SPDX-License-Identifier: GPL-2.0

//! The data types of data/ec/format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{bitfield, CStruct};
use typeinfo_macros::TypeInfo;

#[bitfield(u8)]
pub struct bch_stripe_algorithm_bits {
    #[bits(4)]
    pub algorithm: u8,
    #[bits(1)]
    pub needs_reconcile: u8,
    #[bits(3)]
    pub can_widen: u8,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_stripe {
    pub v: c::bch_val,
    pub sectors: le::U16,
    /*
     * can_widen: how many additional data blocks this stripe could grow
     * into if reused at the current pool's wider target geometry.
     * 3 bits = 0..7, saturating - writers must clamp.
     */
    #[c_bitfield]
    pub algorithm_bits: bch_stripe_algorithm_bits,
    pub nr_blocks: u8,
    pub nr_redundant: u8,

    pub csum_granularity_bits: u8,
    pub csum_type: u8,

    /*
     * XXX: targets should be 16 bits - fix this if we ever do a stripe_v2
     *
     * we can manage with this because this only needs to point to a
     * disk label, not a target:
     */
    pub disk_label: u8,

    /*
     * Variable length sections:
     * - Pointers
     * - Checksums
     *   2D array of [stripe block/device][csum block], with checksum block
     *   size given by csum_granularity_bits
     * - Block sector counts: per-block array of u16s
     *
     * XXX:
     * Either checksums should have come last, or we should have included a
     * checksum_size field (the size in bytes of the checksum itself, not
     * the blocksize the checksum covers).
     *
     * Currently we aren't able to access the block sector counts if the
     * checksum type is unknown.
     */

    pub ptrs: [c::bch_extent_ptr; 0],
}
impl bch_stripe {
    pub fn algorithm(&self) -> u8 { let b = self.algorithm_bits; b.algorithm() }
    pub fn set_algorithm(&mut self, v: u8) { let mut b = self.algorithm_bits; b.set_algorithm(v); self.algorithm_bits = b; }
    pub fn needs_reconcile(&self) -> u8 { let b = self.algorithm_bits; b.needs_reconcile() }
    pub fn set_needs_reconcile(&mut self, v: u8) { let mut b = self.algorithm_bits; b.set_needs_reconcile(v); self.algorithm_bits = b; }
    pub fn can_widen(&self) -> u8 { let b = self.algorithm_bits; b.can_widen() }
    pub fn set_can_widen(&mut self, v: u8) { let mut b = self.algorithm_bits; b.set_can_widen(v); self.algorithm_bits = b; }
}
