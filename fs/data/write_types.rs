// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{NeBytes, c_default};
use core::ffi::c_uint;
use cstruct_macros::{CStruct, bitfield, c_enum, c_xmacro};
use nestify::nest;
use typeinfo_macros::TypeInfo;

c_xmacro! {
    BCH_WRITE_FLAGS(x) {
        (alloc_nowait),
        (cached),
        (data_encoded),
        (pages_stable),
        (pages_owned),
        (only_specified_devs),
        (must_ec),
        (wrote_data_inline),
        (check_enospc),
        (sync),
        (flush),
        (move),
        (replicas_best_effort),
        (in_worker),
        (submitted),
        (convert_unwritten),
    }
}

c_enum! {
    #[closed]
    pub enum __bch_write_flags: u32 {
        BCH_WRITE_FLAGS(f) => __BCH_WRITE_ ## f,
    }
}

c_enum! {
    #[flags]
    pub enum bch_write_flags: u32 {
        BCH_WRITE_FLAGS(f) => BCH_WRITE_ ## f = 1 << (c::__BCH_WRITE_ ## f as u32),
    }
}

nest! {
    #[derive(CStruct, TypeInfo)]*
    #[repr(C)]*
    pub struct bch_write_bio {
        #[c_struct_group]
        pub wbio: pub struct bch_write_bio_wbio {
            pub c: *mut c::bch_fs,
            pub parent: *mut c::bch_write_bio,
            /*
             * Stashed at submit so endio doesn't have to re-derive the dev pointer
             * from @dev — c->devs[@dev] may be NULL by then (dev_remove clears the
             * lookup table before draining refs).  Non-NULL iff we hold an io_ref;
             * doubles as the "we hold the ref" indicator.
             */
            pub ca: *mut c::bch_dev,

            pub submit_time: u64,
            pub inode_offset: u64,
            pub nocow_bucket: u64,

            pub failed: c::bch_io_failures,
            pub dev: u8,

            #[c_bitfield]
            #>[derive(CStruct, TypeInfo)]-
            #>[repr(C)]-
            #>[bitfield(u8, ty = u32)]
            pub bits: pub struct bch_write_bio_bits {
                split:             u1,
                bounce:            u1,
                put_bio:           u1,
                nocow:             u1,
                used_mempool:      u1,
                first_btree_write: u1,
                __pad:             u2,
            },
        },

        pub bio: c::bio,
    }
}
c_default!(bch_write_bio);

nest! {
    #[repr(C)]
    #[derive(CStruct, TypeInfo)]
    pub struct bch_write_op {
        pub cl: c::closure,
        pub c: *mut c::bch_fs,
        pub end_io: Option<unsafe extern "C" fn(*mut c::bch_write_op)>,
        pub start_time: u64,

        /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS, which bcachefs.h defines from these */
        #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
        pub list_idx: c_uint,

        pub written: c_uint, /* sectors */
        pub flags: u16,
        pub error: i16, /* dio write path expects it to hold -ERESTARTSYS... */
        pub io_error: u8,

        #[c_bitfield]
        #>[bitfield(u32, repr = NeBytes<3>, from = NeBytes::<3>::from_u32, into = NeBytes::<3>::to_u32)]
        pub bits: pub struct bch_write_op_bits {
            /*
             * What we want the data to be. Seeded from @opts at init
             * (bch2_write_op_init()), but allowed to diverge from it: the data
             * update path forces an encrypted extent to stay encrypted even if the
             * file's options no longer ask for encryption, so this is the resolved
             * intent for *this* write, not the filesystem's policy.
             *
             * Read these, not @opts, when deciding what to produce.
             */
            compression_opt: u8,
            csum_type:       u4,

            /*
             * Replicas this write will place - not @res.nr_replicas, which is what
             * we reserved space at. An overwrite reserves the delta, and
             * bch2_sum_sector_overwrites() credits back only the old key's
             * uncompressed replicas: a compressed replica holds fewer sectors than
             * it covers. Overwriting a 2 replica extent that has one replica
             * compressed thus reserves for 1 and writes 2.
             */
            nr_replicas:     u4,
            watermark:       u3,
            incompressible:  u1,
            stripe_waited:   u1,
            __pad:           u11,
        },

        pub devs_have: c::bch_devs_list,
        pub target: u16,
        pub nonce: u16,

        /*
         * The fs/file's configured options - policy, not intent. @csum_type and
         * @compression_opt above are what we actually resolved to; where the two
         * disagree, they do so deliberately.
         */
        pub opts: c::bch_inode_opts,

        pub subvol: u32,
        pub pos: c::bpos,
        pub version: c::bversion,

        /*
         * For BCH_WRITE_data_encoded: what the data we're holding currently
         * *is* - as opposed to @csum_type/@compression_opt above, which are
         * what we want it to become. It tracks the buffer: decrypting in place
         * zeroes crc.csum_type because the buffer really is unencrypted and
         * unchecksummed now, and decompressing clears crc.compression_type for
         * the same reason.
         *
         * So the three layers are: @opts is policy, @csum_type is intent, @crc
         * is fact. Anything that changes the buffer must update @crc with it.
         */
        pub crc: c::bch_extent_crc_unpacked,

        pub write_point: c::write_point_specifier,

        pub wp: *mut c::write_point,
        pub wp_list: c::list_head,

        /* Reserved at @res.nr_replicas, which is not @nr_replicas - see above */
        pub res: c::disk_reservation,

        pub open_buckets: c::open_buckets,

        pub new_i_size: u64,
        pub i_sectors_delta: i64,

        pub insert_keys: c::keylist,
        #[c("u64 inline_keys[BKEY_EXTENT_U64s_MAX * 2]")]
        pub inline_keys: [u64; c::BKEY_EXTENT_U64s_MAX * 2],

        /*
         * Bitmask of devices that have had nocow writes issued to them since
         * last flush:
         */
        pub devs_need_flush: *mut c::bch_devs_mask,

        /* Must be last: */
        pub wbio: c::bch_write_bio,
    }
}
c_default!(bch_write_op);
