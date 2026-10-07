// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/accounting_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{c_const, c_enum, c_typedef, c_xmacro, tagged_union, CStruct};
use typeinfo_macros::TypeInfo;

/*
 * Disk accounting - KEY_TYPE_accounting - on disk format:
 *
 * Here, the key has considerably more structure than a typical key (bpos); an
 * accounting key is 'struct disk_accounting_pos', which is a union of bpos.
 *
 * More specifically: a key is just a muliword integer (where word endianness
 * matches native byte order), so we're treating bpos as an opaque 20 byte
 * integer and mapping bch_accounting_key to that.
 *
 * This is a type-tagged union of all our various subtypes; a disk accounting
 * key can be device counters, replicas counters, et cetera - it's extensible.
 *
 * The value is a list of u64s or s64s; the number of counters is specific to a
 * given accounting type.
 *
 * Unlike with other key types, updates are _deltas_, and the deltas are not
 * resolved until the update to the underlying btree, done by btree write buffer
 * flush or journal replay.
 *
 * Journal replay in particular requires special handling. The journal tracks a
 * range of entries which may possibly have not yet been applied to the btree
 * yet - it does not know definitively whether individual entries are dirty and
 * still need to be applied.
 *
 * To handle this, we use the version field of struct bkey, and give every
 * accounting update a unique version number - a total ordering in time; the
 * version number is derived from the key's position in the journal. Then
 * journal replay can compare the version number of the key from the journal
 * with the version number of the key in the btree to determine if a key needs
 * to be replayed.
 *
 * For this to work, we must maintain this strict time ordering of updates as
 * they are flushed to the btree, both via write buffer flush and via journal
 * replay. This has complications for the write buffer code while journal replay
 * is still in progress; the write buffer cannot flush any accounting keys to
 * the btree until journal replay has finished replaying its accounting keys, or
 * the (newer) version number of the keys from the write buffer will cause
 * updates from journal replay to be lost.
 */

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_accounting {
    pub v: c::bch_val,
    pub d: [u64; 0],
}

c_const! {
    #[c_int]
    pub const BCH_ACCOUNTING_MAX_COUNTERS: u32 = 3;
}

c_xmacro! {
    BCH_DATA_TYPES(x) {
        (free, 0),
        (sb, 1),
        (journal, 2),
        (btree, 3),
        (user, 4),
        (cached, 5),
        (parity, 6),
        (stripe, 7),
        (need_gc_gens, 8),
        (need_discard, 9),
        (unstriped, 10),
        (multiple, 11),
    }
}

macro_rules! __bch_data_type_0 {
    ([$($acc:tt)*] $(($t:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_data_type: u32 {
                $($acc)*
                $([<BCH_DATA_ $t>],)*
                BCH_DATA_NR,
            }
        }
    } };
}
BCH_DATA_TYPES!(__bch_data_type_0 []);

c_xmacro! {
    /*
     * field 1: name
     * field 2: id
     * field 3: number of counters (max 3)
     */
    BCH_DISK_ACCOUNTING_TYPES(x) {
        (nr_inodes, 0, 1),
        (persistent_reserved, 1, 1),
        (replicas, 2, 1),
        (dev_data_type, 3, 3),
        (compression, 4, 3),
        (snapshot, 5, 3),
        (btree, 6, 3),
        (rebalance_work, 7, 1),
        (inum, 8, 3),
        (reconcile_work, 9, 2),
        (dev_leaving, 10, 1),
        (stripe_frag, 11, 2),
        (dev_stripe_frag, 12, 2),
    }
}

macro_rules! __disk_accounting_type_0 {
    ([$($acc:tt)*] $(($f:tt, $nr:expr$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum disk_accounting_type: u32 {
                $($acc)*
                $([<BCH_DISK_ACCOUNTING_ $f>] = (($nr) as u32),)*
                BCH_DISK_ACCOUNTING_TYPE_NR,
            }
        }
    } };
}
BCH_DISK_ACCOUNTING_TYPES!(__disk_accounting_type_0 []);

c_typedef! {
    pub type bch_acct_replicas = c::bch_replicas_entry_v1;
}

tagged_union! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[c_typedef]*
    pub struct disk_accounting_pos {
        tag type_: u8 = disk_accounting_type,
        arms from BCH_DISK_ACCOUNTING_TYPES(f, nr, ..) => f: bch_acct_ ## f = nr,
        pad: c::bpos,
        packed,

        /*
         * No subtypes - number of inodes in the entire filesystem
         *
         * XXX: perhaps we could add a per-subvolume counter?
         */
        #[repr(C)]
        pub struct bch_acct_nr_inodes {},

        /*
         * Tracks KEY_TYPE_reservation sectors, broken out by number of replicas for the
         * reservation:
         */
        #[repr(C)]
        pub struct bch_acct_persistent_reserved {
            pub nr_replicas: u8,
        },

        /*
         * device, data type counter fields:
         * [
         *   nr_buckets
         *   live sectors (in buckets of that data type)
         *   sectors of internal fragmentation
         * ]
         *
         * XXX: live sectors should've been done differently, you can have multiple data
         * types in the same bucket (user, stripe, cached) and this collapses them to
         * the bucket data type, and makes the internal fragmentation counter redundant
         *
         * It is possible for live_sectors to be greater than nr_buckets * bucket_size,
         * due to the way compressed disk space accounting works with partially
         * overwritten (and especially split) extents.
         *
         * The fragmentation counter handles this by first clamping bucket sector counts
         * to the bucket size.
         */
        #[repr(C)]
        pub struct bch_acct_dev_data_type {
            pub dev: u8,
            pub data_type: u8,
        },

        /*
         * Compression type fields:
         * [
         *   number of extents
         *   uncompressed size
         *   compressed size
         * ]
         *
         * Compression ratio, average extent size (fragmentation).
         */
        #[repr(C)]
        pub struct bch_acct_compression {
            pub type_: u8,
        },

        /*
         * Per-(snapshot id, btree) accounting, three counters:
         * [
         *   number of keys stamped with this snapshot id in this btree, excluding
         *     whiteouts: snapshot deletion checks this is zero before splicing a node
         *     out of the tree, to catch keys that would otherwise be stranded
         *   total bkey_bytes() of those keys: the in-btree metadata footprint, and an
         *     independent cross-check on the key count
         *   external (on-disk data) sectors: same values as the replicas counter,
         *     aggregated by snapshot id (reservations included); only nonzero for the
         *     extents btree
         * ]
         */
        #[repr(C, packed)]
        pub struct bch_acct_snapshot {
            pub id: u32,
            pub btree: u32,
        },

        /*
         * Metadata accounting per btree id:
         * [
         *   total btree disk usage in sectors
         *   total number of btree nodes
         *   number of non-leaf btree nodes
         * ]
         */
        #[repr(C, packed)]
        pub struct bch_acct_btree {
            pub id: u32,
        },

        /*
         * Simple counter of the amount of data (on disk sectors) rebalance needs to
         * move, extents counted here are also in the rebalance_work btree.
         */
        #[repr(C)]
        pub struct bch_acct_rebalance_work {},

        /*
         * inum counter fields:
         * [
         *   number of extents
         *   sum of extent sizes - bkey size
         *     this field is similar to inode.bi_sectors, except here extents in
         *     different snapshots but the same inode number are all collapsed to the
         *     same counter
         *   sum of on disk size - same values tracked by replicas counters
         * ]
         *
         * This tracks on disk fragmentation.
         */
        #[repr(C, packed)]
        pub struct bch_acct_inum {
            pub inum: u64,
        },

        #[repr(C)]
        pub struct bch_acct_reconcile_work {
            pub type_: u8,
        },

        #[repr(C, packed)]
        pub struct bch_acct_dev_leaving {
            pub dev: u32,
        },

        /*
         * Empty (reusable) blocks in stripes, keyed by how many the stripe has:
         * [
         *   data sectors in such stripes, parity excluded so the two counters share a
         *     denominator
         *   sectors in the empty blocks
         * ]
         *
         * Sectors rather than stripe counts - stripes need not be the same size.
         *
         * This is the part of dev_data_type's `fragmented` that is reusable rather than
         * lost: that counter charges a whole bucket for an emptied stripe member, so
         * without this there is no telling a free slot from a stranded partial block.
         *
         * No upgrade/downgrade entry: informational, so it reads as zero or stale until
         * check_allocations recomputes it.
         */
        #[repr(C)]
        pub struct bch_acct_stripe_frag {
            pub nr_empty: u8,
        },

        /*
         * The same per device:
         * [
         *   data sectors this device holds in stripes
         *   sectors of that which are in empty blocks
         * ]
         *
         * nr_empty is a property of the whole stripe, so a key carrying both it and dev
         * would move every block of a stripe between keys whenever any one emptied -
         * hence two keys, not one.
         */
        #[repr(C)]
        pub struct bch_acct_dev_stripe_frag {
            pub dev: u8,
        },
    }
}
