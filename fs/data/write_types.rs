// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::sb::members::{DevIoRef, IoRefIdx};
use crate::fs::Fs;
use crate::types::NeBytes;
use core::ffi::c_uint;
use cstruct_macros::{MaybeZeroable, Init, bitfield, bch_enum, TypeInfo, BitfieldMembers};
use nestify::nest;
use pin_init::pin_data;
use crate::format::{Bpos, Bversion};

#[bch_enum(closed)]
#[mask(WriteFlags: u32)]
#[repr(u32)]
pub enum WriteFlagBit {
    alloc_nowait,
    cached,
    data_encoded,
    pages_stable,
    pages_owned,
    only_specified_devs,
    must_ec,
    wrote_data_inline,
    check_enospc,
    flush,
    update,
    replicas_best_effort,
    submitted,
    convert_unwritten,
}

nest! {
    #[derive(MaybeZeroable, TypeInfo)]*
    #[repr(C)]*
    /* K, not R: #[pin_data]'s with_project() has a generic named R */
    #[pin_data]
    pub struct bch_write_bio<K: IoRefIdx = c::DevWriteRefKind> {
        #>[derive(BitfieldMembers)]
        pub wbio: pub struct bch_write_bio_wbio {
            /* set at submit: 'static as a write op's fs() is */
            pub c: Option<&'static Fs>,

            pub submit_time: u64,
            pub inode_offset: u64,
            pub nocow_bucket: u64,

            pub dev: u8,

            #[members]
            #>[derive(MaybeZeroable, TypeInfo)]-
            #>[repr(C)]-
            #>[bitfield(u8)]
            pub bits: pub struct bch_write_bio_bits {
                bounce:            u1,
                put_bio:           u1,
                nocow:             u1,
                used_mempool:      u1,
                first_btree_write: u1,
                __pad:             u3,
            },
        },

        /*
         * Stashed at submit so endio doesn't have to re-derive the dev pointer
         * from @dev — c->devs[@dev] may be NULL by then (dev_remove clears the
         * lookup table before draining refs).  Non-NULL iff we hold an io_ref;
         * doubles as the "we hold the ref" indicator.
         */
        pub ca: Option<DevIoRef<K>>,

        #[pin]
        pub bio: crate::util::bio::Bio,
    }
}

impl<K: IoRefIdx> c::bch_write_bio<K> {
    /// A write op's, around the bio and what else its owner set up: no io ref
    /// held - @ca is set at submit, and taken back by the endio. An op
    /// dropped without one submitted drops a @ca of None, not what its
    /// memory last held.
    ///
    /// # Safety
    /// Its bio's initialized by the time its struct's initializer runs.
    pub unsafe fn around_bio() -> impl ::pin_init::Init<Self> {
        unsafe {
            ::pin_init::init_from_closure(|w: *mut Self| {
                (&raw mut (*w).ca).write(None);
                Ok(())
            })
        }
    }
}

// SAFETY: its bio, its last field, where it is in either kind's - below
unsafe impl<R: IoRefIdx> crate::util::bio::HasBio for c::bch_write_bio<R> {
    const BIO_OFFSET: usize = core::mem::offset_of!(c::bch_write_bio, bio);
}

// no thread's, as its bio is: submitted on one, completed on another
unsafe impl<R: IoRefIdx> Send for c::bch_write_bio<R> {}

const _: () = assert!(core::mem::offset_of!(c::bch_write_bio, bio) + size_of::<crate::util::bio::Bio>() ==
                      size_of::<c::bch_write_bio>(), "bch_write_bio: its bio isn't last");

// One pool clones both kinds' bios - REPLICA_CLONES - and to_wbio() finds
// either from its bio as the default's
const _: () = assert!(core::mem::offset_of!(c::bch_write_bio<c::DevReadRefKind>, bio) ==
                      core::mem::offset_of!(c::bch_write_bio, bio));

nest! {
    /* made by init(): bch2_write_op_init() */
    #[pin_data]
    #[repr(C)]
    #[derive(MaybeZeroable, BitfieldMembers, TypeInfo, Init)]
    #[init_args(fs: &Fs, opts: c::bch_inode_opts)]
    pub struct bch_write_op {
        #[init(unsafe { crate::util::backref::BackRef::new(fs) })]
        pub c: crate::util::backref::BackRef<Fs>,
        pub start_time: u64,

        pub written: c_uint, /* sectors */
        pub flags: u16,
        pub error: i16, /* dio write path expects it to hold -ERESTARTSYS... */
        pub io_error: u8,

        #[members]
        #[init(bch_write_op_bits::new()
               .with_csum_type(fs.data_checksum_type(&opts))
               .with_compression_opt(opts.compression)
               .with_watermark(c::Watermark::normal as u8))]
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
            #[bits(4)] csum_type: c::CsumType,

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

        /* Reserved at @res.nr_replicas, which is not @nr_replicas - see above */
        #[init(c::DiskReservation::new(unsafe { crate::util::backref::BackRef::new(fs).get() }))]
        pub res: c::DiskReservation<'static>,

        pub open_buckets: c::open_buckets,

        #[init(u64::MAX)]
        pub new_i_size: u64,
        pub i_sectors_delta: i64,

        #[init(crate::data::keylist::Keylist::new())]
        pub insert_keys: crate::data::keylist::Keylist<{ c::BKEY_EXTENT_U64s_MAX * 2 }>,

        /*
         * Bitmask of devices that have had nocow writes issued to them since
         * last flush:
         */
        pub devs_need_flush: *const c::AtomicDevsMask,

        /* what failed writing this round's buckets: */
        #[init(c::bch_io_failures::default())]
        pub failed: c::bch_io_failures,

        /* c->writes, held till it's done - a data update's holds its own */
        #[init(None)]
        pub write_ref: Option<crate::util::refcount::RefGuard<crate::util::refcount::PercpuRef>>,

        /* Must be last - and its bio set up by the op's owner, before init() */
        #[pin]
        #[init(unsafe { c::bch_write_bio::around_bio() })]
        pub wbio: c::bch_write_bio,
    }
}

// written by its write's task, on whichever thread runs it; what it points
// to - the inode's devs_need_flush - outlives it, and is atomic
unsafe impl Send for c::bch_write_op {}

const _: () = assert!((c::WriteFlagBit::convert_unwritten as u32) < u16::BITS,
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
