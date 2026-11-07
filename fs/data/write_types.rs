// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::fs::Dev;
use crate::cstructs::c;
use crate::fs::Fs;
use crate::types::{NeBytes, c_default};
use core::ffi::c_uint;
use cstruct_macros::{CStruct, Init, bitfield, c_enum, TypeInfo};
use nestify::nest;
use pin_init::pin_data;
use crate::format::{Bpos, Bversion};

c_enum! {
    #[closed]
    #[prefix(__BCH_WRITE_)]
    #[mask(bch_write_flags: u32, prefix = BCH_WRITE_)]
    pub enum __bch_write_flags: u32 {
        alloc_nowait,
        cached,
        data_encoded,
        pages_stable,
        pages_owned,
        only_specified_devs,
        must_ec,
        wrote_data_inline,
        check_enospc,
        sync,
        flush,
        __BCH_WRITE_move,
        replicas_best_effort,
        in_worker,
        submitted,
        convert_unwritten,
    }
}

nest! {
    #[derive(CStruct, TypeInfo)]*
    #[repr(C)]*
    pub struct bch_write_bio {
        #[c_struct_group]
        pub wbio: pub struct bch_write_bio_wbio {
            /* set at submit: 'static as a write op's fs() is */
            #[c("struct bch_fs *c")]
            pub c: Option<&'static Fs>,
            pub parent: *mut c::bch_write_bio,
            /*
             * Stashed at submit so endio doesn't have to re-derive the dev pointer
             * from @dev — c->devs[@dev] may be NULL by then (dev_remove clears the
             * lookup table before draining refs).  Non-NULL iff we hold an io_ref;
             * doubles as the "we hold the ref" indicator.
             */
            #[c("struct bch_dev *ca")]
            pub ca: Option<&'static Dev>,

            pub submit_time: u64,
            pub inode_offset: u64,
            pub nocow_bucket: u64,

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

crate::has_bio!(c::bch_write_bio, bio);

nest! {
    /* made by init(): bch2_write_op_init() */
    #[pin_data]
    #[repr(C)]
    #[derive(CStruct, TypeInfo, Init)]
    #[init_args(fs: &Fs, opts: c::bch_inode_opts)]
    pub struct bch_write_op {
        #[c("struct closure cl")]
        pub cl: crate::util::ffi::Opaque<c::closure>,
        #[c("struct bch_fs *c")]
        #[init(unsafe { crate::util::backref::BackRef::new(fs) })]
        pub c: crate::util::backref::BackRef<Fs>,
        #[c("void (*end_io)(struct bch_write_op *)")]
        pub end_io: Option<unsafe fn(*mut c::bch_write_op)>,
        pub start_time: u64,

        /* its slot on its async object list */
        pub list_idx: c_uint,

        pub written: c_uint, /* sectors */
        pub flags: u16,
        pub error: i16, /* dio write path expects it to hold -ERESTARTSYS... */
        pub io_error: u8,

        #[c_bitfield]
        #[init(bch_write_op_bits::new()
               .with_csum_type(fs.data_checksum_type(&opts).0 as u8)
               .with_compression_opt(opts.compression)
               .with_watermark(c::bch_watermark::normal as u8))]
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
         *
         * TODO: this should be the compact reconcile opts, not a full
         * bch_inode_opts - the write path doesn't need foreground vs background
         * compression, its caller should be choosing that. Then the read bio
         * can carry the effective opts per extent too, and
         * read_extent_rbio_alloc() stops looking up a reflink_v's own.
         */
        #[init(opts)]
        pub opts: c::bch_inode_opts,

        pub subvol: u32,
        #[init(crate::POS_MAX)]
        pub pos: Bpos,
        pub version: Bversion,

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

        /* the filesystem's, so 'static: as the op's fs() */
        #[c("struct write_point *wp")]
        pub wp: Option<&'static c::write_point>,
        pub wp_list: c::list_head,

        /* Reserved at @res.nr_replicas, which is not @nr_replicas - see above */
        #[init(c::DiskReservation::new(unsafe { crate::util::backref::BackRef::new(fs).get() }))]
        pub res: c::disk_reservation,

        pub open_buckets: c::open_buckets,

        #[init(u64::MAX)]
        pub new_i_size: u64,
        pub i_sectors_delta: i64,

        #[c("u64 insert_keys[1 + BKEY_EXTENT_U64s_MAX * 2]")]
        #[init(crate::data::keylist::Keylist::new())]
        pub insert_keys: crate::data::keylist::Keylist<{ c::BKEY_EXTENT_U64s_MAX * 2 }>,

        /*
         * Bitmask of devices that have had nocow writes issued to them since
         * last flush:
         */
        #[c("struct bch_devs_mask *devs_need_flush")]
        pub devs_need_flush: *const c::AtomicDevsMask,

        /* what failed writing this round's buckets: */
        #[init(c::bch_io_failures::default())]
        pub failed: c::bch_io_failures,

        /* c->writes, held till it's done - a data update's holds its own */
        #[c("void *write_ref")]
        #[init(None)]
        pub write_ref: Option<crate::util::refcount::RefGuard<crate::util::enumerated_ref::EnumeratedRef>>,

        /* Must be last - and its bio set up by the op's owner, before init() */
        #[init(unsafe { crate::util::alloc::untouched() })]
        pub wbio: c::bch_write_bio,
    }
}

crate::has_closure!(c::bch_write_op, cl);

/// A write point's writes, waiting for their index updates.
pub type WriteOpList = crate::list!(c::bch_write_op, wp_list);

const _: () = assert!((c::__bch_write_flags::convert_unwritten as u32) < u16::BITS,
                      "the write flags don't fit in bch_write_op's flags");

impl c::bch_write_op {
    /// The op's filesystem, as 'static: an op is done before its filesystem
    /// goes, and what it hands out of it goes no further than the op.
    pub fn fs(&self) -> &'static Fs {
        // SAFETY: as above
        unsafe { self.c.get() }
    }

    /// An op for @fs, with @opts, made at @op by init(): its wbio's bio left as
    /// its owner set it up.
    ///
    /// # Safety
    /// @fs outlives the op, and @op holds nothing to drop: new memory, or an
    /// op whose write's done - its reservation put, its keys and refs gone.
    pub unsafe fn init_at(op: *mut Self, fs: &Fs, opts: c::bch_inode_opts) {
        unsafe { crate::util::alloc::init_at(Self::init(fs, opts), op) }
            .unwrap_or_else(|e| panic!("a write op's init, which can't fail: {e}"));
    }
}
