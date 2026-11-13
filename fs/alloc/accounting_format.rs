// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::vstructs::Head;
use cstruct_macros::{c_name, bch_enum, counters, tagged_union, whole, CStruct, Plain, TypeInfo};
use crate::format::Bpos;

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

#[whole(tail = d: [u64], bkey)]
#[repr(C)]
#[c_name(bch_accounting)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct Accounting {
    pub v: c::bch_val,
    pub d: [u64; 0],
}

pub const BCH_ACCOUNTING_MAX_COUNTERS: u32 = 3;

#[bch_enum(open)]
#[c_name(bch_data_type)]
#[prefix(BCH_DATA_)]
#[nr]
#[mask(DataTypes: u16, prefix = BCH_DATA_MASK_, c_name = bch_data_types)]
#[repr(u32)]
pub enum DataType {
    free = 0,
    sb = 1,
    journal = 2,
    btree = 3,
    user = 4,
    cached = 5,
    parity = 6,
    stripe = 7,
    need_gc_gens = 8,
    need_discard = 9,
    unstriped = 10,
    multiple = 11,
}

/// C's name, as bindgen's declarations of C functions spell it.
pub type bch_data_type = DataType;

impl From<DataType> for DataTypes {
    fn from(t: DataType) -> Self {
        Self::from_bits_retain(1 << t.0)
    }
}

pub type Replicas = Head<c::bch_replicas_entry_v1>;
crate::accounting::acct_counters!(ReplicasCounters { sectors });

tagged_union! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[c_name(disk_accounting_pos)]
    #[derive(TypeInfo)]
    #[tag(type_: u8)]
    #[tag_enum(disk_accounting_type: u32, prefix = BCH_DISK_ACCOUNTING_, nr)]
    #[pad(Bpos)]
    #[size(pad)]
    #[packed]
    pub enum DiskAccountingPos {
        /*
         * No subtypes - number of inodes in the entire filesystem
         *
         * XXX: perhaps we could add a per-subvolume counter?
         */
        nr_inodes(
            #[repr(C)]
            #[c_name(bch_acct_nr_inodes)]
            #[counters(nr)]
            pub struct NrInodes {}
        ) = 0,

        /*
         * Tracks KEY_TYPE_reservation sectors, broken out by number of replicas for the
         * reservation:
         */
        persistent_reserved(
            #[repr(C)]
            #[c_name(bch_acct_persistent_reserved)]
            #[counters(sectors)]
            pub struct PersistentReserved {
                pub nr_replicas: u8,
            }
        ) = 1,

        replicas(Replicas) = 2,

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
        dev_data_type(
            #[repr(C)]
            #[c_name(bch_acct_dev_data_type)]
            #[counters(buckets, sectors, fragmented)]
            pub struct DevDataType {
                pub dev: u8,
                pub data_type: u8,
            }
        ) = 3,

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
        compression(
            #[repr(C)]
            #[c_name(bch_acct_compression)]
            #[counters(nr_extents, uncompressed, compressed)]
            pub struct Compression {
                pub type_: u8,
            }
        ) = 4,

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
        snapshot(
            #[repr(C, packed)]
            #[c_name(bch_acct_snapshot)]
            #[counters(keys, key_bytes, sectors)]
            pub struct Snapshot {
                pub id: u32,
                pub btree: u32,
            }
        ) = 5,

        /*
         * Metadata accounting per btree id:
         * [
         *   total btree disk usage in sectors
         *   total number of btree nodes
         *   number of non-leaf btree nodes
         * ]
         */
        btree(
            #[repr(C, packed)]
            #[c_name(bch_acct_btree)]
            #[counters(sectors, nodes, interior)]
            pub struct Btree {
                pub id: u32,
            }
        ) = 6,

        /*
         * Simple counter of the amount of data (on disk sectors) rebalance needs to
         * move, extents counted here are also in the rebalance_work btree.
         */
        rebalance_work(
            #[repr(C)]
            #[c_name(bch_acct_rebalance_work)]
            #[counters(sectors)]
            pub struct RebalanceWork {}
        ) = 7,

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
        inum(
            #[repr(C, packed)]
            #[c_name(bch_acct_inum)]
            #[counters(nr_extents, sectors, disk_sectors)]
            pub struct Inum {
                pub inum: u64,
            }
        ) = 8,

        reconcile_work(
            #[repr(C)]
            #[c_name(bch_acct_reconcile_work)]
            #[counters(data, metadata)]
            pub struct ReconcileWork {
                pub type_: u8,
            }
        ) = 9,

        dev_leaving(
            #[repr(C, packed)]
            #[c_name(bch_acct_dev_leaving)]
            #[counters(sectors)]
            pub struct DevLeaving {
                pub dev: u32,
            }
        ) = 10,

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
        stripe_frag(
            #[repr(C)]
            #[c_name(bch_acct_stripe_frag)]
            #[counters(data, empty)]
            pub struct StripeFrag {
                pub nr_empty: u8,
            }
        ) = 11,

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
        dev_stripe_frag(
            #[repr(C)]
            #[c_name(bch_acct_dev_stripe_frag)]
            #[counters(data, empty)]
            pub struct DevStripeFrag {
                pub dev: u8,
            }
        ) = 12,
    }
}
