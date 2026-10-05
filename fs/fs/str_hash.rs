// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{bkey_s_c_to_result, BtreeIter, BtreeIterFlags, BtreeTrans, UpdateTriggerFlags};
use crate::c;
use crate::check::SnapshotsSeen;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::printbuf_to_formatter;
use core::ffi::c_void;
use core::fmt;

/// A btree that's a hash table keyed by name - dirents, xattrs: what C's
/// struct bch_hash_desc describes, as a trait.
///
/// The C descriptor is a table of the hash and compare functions, passed by
/// value into static inlines so each table's code is specialized - a trait,
/// written by hand. Until str_hash.c is Rust, desc() hands the C table to the
/// C functions; then those functions become methods here, and desc() goes.
pub trait HashTable {
    /// What a lookup searches for: a name, or for xattrs a name and type.
    type Key;

    fn desc() -> &'static c::bch_hash_desc;

    /// Where @k's name hashes to under @info: desc->hash_bkey().
    fn hash_bkey(info: &c::bch_hash_info, k: BkeySC<'_>) -> u64 {
        unsafe { (Self::desc().hash_bkey.expect("hash_bkey"))(info, k.to_raw()) }
    }

    /// Whether @a and @b have the same name: !desc->cmp_bkey().
    fn same_name(a: BkeySC<'_>, b: BkeySC<'_>) -> bool {
        unsafe { !(Self::desc().cmp_bkey.expect("cmp_bkey"))(a.to_raw(), b.to_raw()) }
    }
}

/// @inode's hash info, without bch2_hash_info_init()'s check that casefolding
/// is enabled: for comparing hash info, not hashing with it - as
/// __bch2_hash_info_init().
pub fn hash_info_init_unchecked(fs: &Fs, inode: &c::bch_inode_unpacked) -> c::bch_hash_info {
    unsafe { c::__bch2_hash_info_init(fs.raw, inode) }
}

impl PartialEq for c::SIPHASH_KEY {
    fn eq(&self, other: &Self) -> bool {
        self.k0 == other.k0 && self.k1 == other.k1
    }
}

/// Field by field: memcmp() would compare padding.
impl PartialEq for c::bch_hash_info {
    fn eq(&self, other: &Self) -> bool {
        self.inum_snapshot == other.inum_snapshot &&
        self.type_         == other.type_ &&
        self.is_31bit      == other.is_31bit &&
        self.cf_encoding   == other.cf_encoding &&
        self.siphash_key   == other.siphash_key
    }
}

/// Insert @insert into hash table @T, in inode @inum as seen in @snapshot -
/// unless a key of the same name is already there: as
/// bch2_hash_set_or_get_in_snapshot(), with STR_HASH_MUST_CREATE in
/// @iter_flags. None if it was inserted (queued), else the key that was
/// there, through @iter.
#[allow(clippy::too_many_arguments)]
pub fn set_or_get_in_snapshot<'i, T: HashTable>(
    trans:        &BtreeTrans<'_>,
    iter:         &'i mut BtreeIter<'_>,
    hash_info:    &c::bch_hash_info,
    inum:         c::subvol_inum,
    snapshot:     u32,
    insert:       &mut c::bkey_i,
    iter_flags:   BtreeIterFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<Option<BkeySC<'i>>, BchError> {
    let flags = iter_flags.bits() | update_flags.bits();
    let k = unsafe {
        c::bch2_hash_set_or_get_in_snapshot(trans.raw(), iter.raw_mut(), *T::desc(), hash_info,
                                            inum, snapshot, insert,
                                            c::btree_iter_update_trigger_flags(flags))
    };
    bkey_s_c_to_result(k)
}

/// A str_hash type - INODE_STR_HASH() - for formatting with {}: as
/// bch2_prt_str_hash_type().
pub struct StrHashType(pub u64);

impl fmt::Display for StrHashType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use c::bch_str_hash_type::*;

        // A Rust enum: an out of range value can't be converted, and the
        // inode field this comes from is four bits
        let t = match self.0 {
            0 => BCH_STR_HASH_crc32c,
            1 => BCH_STR_HASH_crc64,
            2 => BCH_STR_HASH_siphash_old,
            3 => BCH_STR_HASH_siphash,
            n => return write!(f, "(invalid str_hash type {n})"),
        };
        printbuf_to_formatter(f, |out| unsafe { c::bch2_prt_str_hash_type(out, t) })
    }
}

/// Look up @key in hash table @T, in inode @inum as seen in @snapshot: as
/// bch2_hash_lookup_in_snapshot(). The key found, through @iter; ENOENT if
/// there is none.
pub fn lookup_in_snapshot<'i, T: HashTable>(
    trans:     &BtreeTrans<'_>,
    iter:      &'i mut BtreeIter<'_>,
    hash_info: &c::bch_hash_info,
    inum:      c::subvol_inum,
    key:       &T::Key,
    flags:     BtreeIterFlags,
    snapshot:  u32,
) -> Result<BkeySC<'i>, BchError> {
    let k = unsafe {
        c::bch2_hash_lookup_in_snapshot(trans.raw(), iter.raw_mut(), *T::desc(), hash_info, inum,
                                        key as *const T::Key as *const c_void,
                                        c::btree_iter_update_trigger_flags(flags.bits()), snapshot)
    };
    Ok(bkey_s_c_to_result(k)?.expect("a hash lookup returns a key or an error"))
}

/// Make @inode's hash info - seed and type - match @snapshot_root's, which
/// every version of an inode shares: as bch2_repair_inode_hash_info().
/// Commits, and returns a restart.
pub fn repair_inode_hash_info(
    trans:         &BtreeTrans<'_>,
    inode:         &mut c::bch_inode_unpacked,
    snapshot_root: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_repair_inode_hash_info(trans.raw(), inode, snapshot_root) })
}

pub fn hash_info_init(
    fs:    &Fs,
    inode: &c::bch_inode_unpacked,
) -> Result<c::bch_hash_info, BchError> {
    let mut hash_info: c::bch_hash_info = Default::default();
    ret_to_result(unsafe {
        c::bch2_hash_info_init(fs.raw, inode, &mut hash_info)
    })?;
    Ok(hash_info)
}

/// Delete the key at @iter from hash table @T, leaving a whiteout if a later
/// key in the same probe sequence needs one: as bch2_hash_delete_at().
pub fn delete_at<T: HashTable>(
    trans:     &BtreeTrans<'_>,
    hash_info: &c::bch_hash_info,
    iter:      &mut BtreeIter<'_>,
    flags:     UpdateTriggerFlags,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_hash_delete_at(trans.raw(), *T::desc(), hash_info, iter.raw_mut(),
                               c::btree_iter_update_trigger_flags(flags.bits()))
    })
}

/// Check that @k, in hash table @T, is where its hash says it should be,
/// repairing it if not: as bch2_str_hash_check_key(). @s, for the btrees whose
/// keys are walked with all snapshots in fsck, gives the snapshot visibility
/// for repairs; a repair that moves a key to before @k's position sets
/// @updated_before_k_pos.
///
/// A mismatched @hash_info - an inode version whose hash seed or type differs
/// from its snapshot root's - is repaired and committed, and returned as a
/// restart: whatever the caller has cached about the inode is stale then.
pub fn check_key<T: HashTable>(
    trans:               &BtreeTrans<'_>,
    s:                   Option<&mut SnapshotsSeen>,
    hash_info:           &mut c::bch_hash_info,
    k:                   BkeySC<'_>,
    updated_before_k_pos: &mut bool,
) -> Result<(), BchError> {
    // Only ever set on the way to an error: the restart says it.
    let mut repaired_inode = false;

    ret_to_result(unsafe {
        c::bch2_str_hash_check_key(trans.raw(),
                                   s.map_or(core::ptr::null_mut(), |s| s.raw_mut()),
                                   T::desc(), hash_info, k.to_raw(),
                                   updated_before_k_pos, &mut repaired_inode)
    })
}
