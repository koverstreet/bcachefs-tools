// SPDX-License-Identifier: GPL-2.0

//! The data types of data/write_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, bitfield, c_enum, c_extern, c_xmacro};
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

macro_rules! ____bch_write_flags_0 {
    ([$($acc:tt)*] $(($f:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum __bch_write_flags: u32 {
                $($acc)*
                $([<__BCH_WRITE_ $f>],)*
            }
        }
    } };
}
BCH_WRITE_FLAGS!(____bch_write_flags_0 []);

macro_rules! __bch_write_flags_0 {
    ([$($acc:tt)*] $(($f:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[flags]
            pub enum bch_write_flags: u32 {
                $($acc)*
                $([<BCH_WRITE_ $f>] = 1 << (c::[<__BCH_WRITE_ $f>] as u32),)*
            }
        }
    } };
}
BCH_WRITE_FLAGS!(__bch_write_flags_0 []);

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
            #>[bitfield(u8)]
            pub split_bits: pub struct bch_write_bio_split_bits {
                #[bits(1)]
                pub split: u32,
                #[bits(1)]
                pub bounce: u32,
                #[bits(1)]
                pub put_bio: u32,
                #[bits(1)]
                pub nocow: u32,
                #[bits(1)]
                pub used_mempool: u32,
                #[bits(1)]
                pub first_btree_write: u32,
                #[bits(2)]
                pub __pad: u8,
            },
        },

        pub bio: c::bio,
    }
}
c_default!(bch_write_bio);
impl bch_write_bio_wbio {
    pub fn split(&self) -> u32 { let b = self.split_bits; b.split() }
    pub fn set_split(&mut self, v: u32) { let mut b = self.split_bits; b.set_split(v); self.split_bits = b; }
    pub fn bounce(&self) -> u32 { let b = self.split_bits; b.bounce() }
    pub fn set_bounce(&mut self, v: u32) { let mut b = self.split_bits; b.set_bounce(v); self.split_bits = b; }
    pub fn put_bio(&self) -> u32 { let b = self.split_bits; b.put_bio() }
    pub fn set_put_bio(&mut self, v: u32) { let mut b = self.split_bits; b.set_put_bio(v); self.split_bits = b; }
    pub fn nocow(&self) -> u32 { let b = self.split_bits; b.nocow() }
    pub fn set_nocow(&mut self, v: u32) { let mut b = self.split_bits; b.set_nocow(v); self.split_bits = b; }
    pub fn used_mempool(&self) -> u32 { let b = self.split_bits; b.used_mempool() }
    pub fn set_used_mempool(&mut self, v: u32) { let mut b = self.split_bits; b.set_used_mempool(v); self.split_bits = b; }
    pub fn first_btree_write(&self) -> u32 { let b = self.split_bits; b.first_btree_write() }
    pub fn set_first_btree_write(&mut self, v: u32) { let mut b = self.split_bits; b.set_first_btree_write(v); self.split_bits = b; }
}

#[bitfield(u32, repr = crate::types::NeBytes::<3>, from = crate::types::NeBytes::<3>::from_u32, into = crate::types::NeBytes::<3>::to_u32)]
pub struct bch_write_op_compression_opt_bits {
    /*
     * What we want the data to be. Seeded from @opts at init
     * (bch2_write_op_init()), but allowed to diverge from it: the data
     * update path forces an encrypted extent to stay encrypted even if the
     * file's options no longer ask for encryption, so this is the resolved
     * intent for *this* write, not the filesystem's policy.
     *
     * Read these, not @opts, when deciding what to produce.
     */
    #[bits(8)]
    pub compression_opt: u32,
    #[bits(4)]
    pub csum_type: u32,

    /*
     * Replicas this write will place - not @res.nr_replicas, which is what
     * we reserved space at. An overwrite reserves the delta, and
     * bch2_sum_sector_overwrites() credits back only the old key's
     * uncompressed replicas: a compressed replica holds fewer sectors than
     * it covers. Overwriting a 2 replica extent that has one replica
     * compressed thus reserves for 1 and writes 2.
     */
    #[bits(4)]
    pub nr_replicas: u32,
    #[bits(3)]
    pub watermark: u32,
    #[bits(1)]
    pub incompressible: u32,
    #[bits(1)]
    pub stripe_waited: u32,
    #[bits(11)]
    pub __pad: u16,
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_write_op {
    pub cl: c::closure,
    pub c: *mut c::bch_fs,
    pub end_io: Option<unsafe extern "C" fn(*mut c::bch_write_op)>,
    pub start_time: u64,

    /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS: see types.rs */
    #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
    pub list_idx: core::ffi::c_uint,

    pub written: core::ffi::c_uint, /* sectors */
    pub flags: u16,
    pub error: i16, /* dio write path expects it to hold -ERESTARTSYS... */
    pub io_error: u8,

    #[c_bitfield]
    pub compression_opt_bits: bch_write_op_compression_opt_bits,

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
c_default!(bch_write_op);
impl bch_write_op {
    pub fn compression_opt(&self) -> u32 { let b = self.compression_opt_bits; b.compression_opt() }
    pub fn set_compression_opt(&mut self, v: u32) { let mut b = self.compression_opt_bits; b.set_compression_opt(v); self.compression_opt_bits = b; }
    pub fn csum_type(&self) -> u32 { let b = self.compression_opt_bits; b.csum_type() }
    pub fn set_csum_type(&mut self, v: u32) { let mut b = self.compression_opt_bits; b.set_csum_type(v); self.compression_opt_bits = b; }
    pub fn nr_replicas(&self) -> u32 { let b = self.compression_opt_bits; b.nr_replicas() }
    pub fn set_nr_replicas(&mut self, v: u32) { let mut b = self.compression_opt_bits; b.set_nr_replicas(v); self.compression_opt_bits = b; }
    pub fn watermark(&self) -> u32 { let b = self.compression_opt_bits; b.watermark() }
    pub fn set_watermark(&mut self, v: u32) { let mut b = self.compression_opt_bits; b.set_watermark(v); self.compression_opt_bits = b; }
    pub fn incompressible(&self) -> u32 { let b = self.compression_opt_bits; b.incompressible() }
    pub fn set_incompressible(&mut self, v: u32) { let mut b = self.compression_opt_bits; b.set_incompressible(v); self.compression_opt_bits = b; }
    pub fn stripe_waited(&self) -> u32 { let b = self.compression_opt_bits; b.stripe_waited() }
    pub fn set_stripe_waited(&mut self, v: u32) { let mut b = self.compression_opt_bits; b.set_stripe_waited(v); self.compression_opt_bits = b; }
}

// What Rust calls of data/write.h: C gets these as prototypes, in data/write_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    // CLOSURE_CALLBACK(bch2_write)
    pub fn bch2_write(ws: *mut c::work_struct);
}
