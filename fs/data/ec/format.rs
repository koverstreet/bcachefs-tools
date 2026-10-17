// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le16;
use cstruct_macros::{bitfield, whole, CStruct, Plain, TypeInfo};
use nestify::nest;

nest! {
    #[whole(tail = ptrs: [u8], bkey)]
    #[repr(C, align(8))]
    #[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
    #[c_packed]
    #[c_name(bch_stripe)]
    pub struct Stripe {
        pub v: c::bch_val,
        pub sectors: Le16,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct bch_stripe_bits {
            algorithm:       u4,
            needs_reconcile: u1,
            /*
             * can_widen: how many additional data blocks this stripe could grow
             * into if reused at the current pool's wider target geometry.
             * 3 bits = 0..7, saturating - writers must clamp.
             */
            can_widen:       u3,
        },
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
}
