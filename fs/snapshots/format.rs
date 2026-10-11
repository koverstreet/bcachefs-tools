// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{c_bitmask, c_const, c_enum, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

c_const! {
    #[c_int]
    pub const BCACHEFS_ROOT_SUBVOL: u32 = 1;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_subvolume {
    pub v: c::bch_val,
    pub flags: le::U32,
    pub snapshot: le::U32,
    pub inode: le::U64,
    /*
     * Snapshot subvolumes form a tree, separate from the snapshot nodes
     * tree - if this subvolume is a snapshot, this is the ID of the
     * subvolume it was created from:
     *
     * This is _not_ necessarily the subvolume of the directory containing
     * this subvolume:
     */
    pub creation_parent: le::U32,
    pub fs_path_parent: le::U32,
    pub otime: c::bch_le128,

    pub state: le::U32,
    pub pad: le::U32,
}

c_xmacro! {
    /*
     * Subvolume lifecycle state: same codeword scheme as bch_snapshot.state
     * (below), additionally >= 14 bit flips from every snapshot state so a value
     * copied across key types reads as garbage, not a legal state:
     *
     * unlinked: unlinked from filesystem tree but still has open files
     * deleted:  no longer referenced, delete_dead_snapshots may delete
     */
    BCH_SUBVOLUME_STATES(x) {
        (live, 0x4ad5447e),
        (unlinked, 0x2d358e8f),
        (deleted, 0x3c6b2d4c),
    }
}

c_enum! {
    #[open]
    pub enum bch_subvolume_state: u32 {
        BCH_SUBVOLUME_STATES(n, v) => SUBVOLUME_STATE_ ## n = v,
    }
}

c_bitmask! {
    LE32_BITMASK(struct bch_subvolume, flags), strip BCH_SUBVOLUME_ {
        BCH_SUBVOLUME_RO(0, 1),
        /*
         * We need to know whether a subvolume is a snapshot so we can know whether we
         * can delete it (or whether it should just be rm -rf'd)
         */
        BCH_SUBVOLUME_SNAP(1, 2),
        /* Obsolete */
        BCH_SUBVOLUME_UNLINKED_OBSOLETE(2, 3),
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_snapshot {
    pub v: c::bch_val,
    pub flags: le::U32,
    pub parent: le::U32,
    pub children: [le::U32; 2],
    pub subvol: le::U32,
    /* corresponds to a bch_snapshot_tree in BTREE_ID_snapshot_trees */
    pub tree: le::U32,
    pub depth: le::U32,
    pub skip: [le::U32; 3],
    pub btime: c::bch_le128,

    pub state: le::U32,
    pub pad: le::U32,
}

c_xmacro! {
    /*
     * WILL_DELETE: leaf node that's no longer referenced by a subvolume, still has
     * keys, will be deleted by delete_dead_snapshots
     *
     * SUBVOL: true if a subvol points to this snapshot (why do we have this?
     * subvols are nonzero)
     *
     * DELETED: we never delete snapshot keys, we mark them as deleted so that we
     * can distinguish between a key for a missing snapshot (and we have no idea
     * what happened) and a key for a deleted snapshot (delete_dead_snapshots() missed
     * something, key should be deleted)
     *
     * NO_KEYS: we don't remove interior snapshot nodes from snapshot trees at
     * runtime, since we can't do the adjustements for the depth/skiplist field
     * atomically - and that breaks e.g. is_ancestor(). Instead, we mark it to be
     * deleted at the next remount; this tells us that we don't need to run the full
     * delete_dead_snapshots().
     *
     *
     * XXX - todo item:
     *
     * We should guard against a bitflip causing us to delete a snapshot incorrectly
     * by cross checking with the subvolume btree: delete_dead_snapshots() can take
     * out more data than any other codepath if it runs incorrectly
     */
    /*
     * The state field licenses the most destructive thing the filesystem can do -
     * deleting user data - so corruption of it must be detectable, never
     * misinterpretable. Every state is a randomly generated codeword: a stray
     * memory stomper almost never lands on a legal state (4 legal values in a
     * 2^32 space; zero and all-ones are illegal, so a wiped field is detected,
     * not misread), and any two states are >= 14 bit flips apart (enforced
     * below), so sparse bit corruption can't turn one legal state into another.
     */
    BCH_SNAPSHOT_STATES(x) {
        (live, 0x757c47a2),
        (will_delete, 0x3316a8d2),
        (no_keys, 0x372b5a01),
        (deleted, 0x7cd4a225),
    }
}

c_enum! {
    #[open]
    pub enum bch_snapshot_state: u32 {
        BCH_SNAPSHOT_STATES(n, v) => SNAPSHOT_STATE_ ## n = v,
    }
}

// The state codewords' distances, as the comment on BCH_SNAPSHOT_STATES
// says: every snapshot and subvolume state at least 14 bit flips from every
// other, of either kind, and 14 to 18 from a zeroed field and from an
// all-ones one. Every pair, from the lists - a state added is checked.
macro_rules! codewords {
    ([] $(($n:tt, $v:expr)),* $(,)?) => { &[$($v),*] };
}
const _: () = {
    let lists: [&[u32]; 2] = [BCH_SNAPSHOT_STATES!(codewords []), BCH_SUBVOLUME_STATES!(codewords [])];
    let mut a = 0;
    while a < lists.len() {
        let mut i = 0;
        while i < lists[a].len() {
            let v = lists[a][i];
            assert!(v.count_ones() >= 14 && v.count_ones() <= 18,
                    "a state codeword within 14 bit flips of zero or of all ones");
            let mut b = a;
            while b < lists.len() {
                let mut j = if b == a { i + 1 } else { 0 };
                while j < lists[b].len() {
                    assert!((v ^ lists[b][j]).count_ones() >= 14,
                            "two state codewords within 14 bit flips of each other");
                    j += 1;
                }
                b += 1;
            }
            i += 1;
        }
        a += 1;
    }
};

c_bitmask! {
    LE32_BITMASK(struct bch_snapshot, flags), strip BCH_SNAPSHOT_ {
        /* Obsolete */
        BCH_SNAPSHOT_WILL_DELETE_OBSOLETE(0, 1),
        BCH_SNAPSHOT_SUBVOL_OBSOLETE(1, 2),
        BCH_SNAPSHOT_DELETED_OBSOLETE(2, 3),
        BCH_SNAPSHOT_NO_KEYS_OBSOLETE(3, 4),
    }
}

/*
 * Snapshot trees:
 *
 * The snapshot_trees btree gives us persistent indentifier for each tree of
 * bch_snapshot nodes, and allow us to record and easily find the root/master
 * subvolume that other snapshots were created from:
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_snapshot_tree {
    pub v: c::bch_val,
    pub master_subvol: le::U32,
    pub root_snapshot: le::U32,
}
