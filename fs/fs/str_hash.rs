// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::{pos, spos, BkeySC, SPOS_MAX};
use crate::btree::iter::{
    polonius_key, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, TransBkey,
    UpdateTriggerFlags,
};
use crate::c;
use crate::check::{self, SnapshotsSeen};
use crate::dirent::{self, Dirent, DirentTarget, Dirents};
use crate::errcode::{bch_errcode, ret_to_result_void as ret_to_result, BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::inode;
use crate::printbuf_to_formatter;
use crate::snapshots::{snapshot, subvolume};
use crate::util::os_str::OsStrExt;
use crate::util::Printbuf;
use crate::{bch_err, fsck_err, fsck_err_on, inode_fsck_err};
use core::ffi::c_void;
use core::fmt;

/// A btree that's a hash table keyed by name - dirents, xattrs: what C's
/// struct bch_hash_desc described. The keys passed to these are valid -
/// from the btree, or built to be inserted.
pub trait HashTable {
    /// What a lookup searches for: a name, or for xattrs a name and type -
    /// which borrows the name, for 'k.
    type Key<'k>: ?Sized;

    const BTREE:    c::btree_id;
    const KEY_TYPE: c::bch_bkey_type;

    /// Where @key hashes to under @info.
    fn hash_key(info: &c::bch_hash_info, key: &Self::Key<'_>) -> u64;

    /// Where @k's name hashes to under @info.
    fn hash_bkey(info: &c::bch_hash_info, k: BkeySC<'_>) -> u64;

    /// Whether @k is what @key looks for.
    fn matches(k: BkeySC<'_>, key: &Self::Key<'_>) -> bool;

    /// Whether @a and @b have the same name.
    fn same_name(a: BkeySC<'_>, b: BkeySC<'_>) -> bool;

    /// Whether @k is in the table as subvolume @inum.subvol sees it.
    fn is_visible(_inum: c::subvol_inum, _k: BkeySC<'_>) -> bool {
        true
    }
}

/// @parts, one after another, hashed under @info: bch2_str_hash_init(),
/// bch2_str_hash_update() for each, bch2_str_hash_end(). With @maybe_31bit,
/// the hash is cut to fit a directory with 31 bit offsets.
pub fn hash_parts(info: &c::bch_hash_info, parts: &[&[u8]], maybe_31bit: bool) -> u64 {
    let mut ctx = c::bch_str_hash_ctx::default();

    unsafe {
        c::bch2_str_hash_init(&mut ctx, info);
        for p in parts {
            c::bch2_str_hash_update(&mut ctx, info, p.as_ptr() as *const c_void, p.len());
        }
        c::bch2_str_hash_end(&mut ctx, info, maybe_31bit)
    }
}

// ── The hash table operations ────────────────────────────────────────────
//
// As str_hash.h's. A key's slot is where its name hashes to, or the first
// after it, probing past collisions and whiteouts: a hole ends a probe
// sequence, which is why deleting leaves a whiteout when a later key in the
// sequence would be cut off.

/// Whether @k is one of @T's keys, visible to subvolume @inum.subvol - all of
/// them are, with no inode number: is_visible_key().
fn is_visible_key<T: HashTable>(inum: c::subvol_inum, k: BkeySC<'_>) -> bool {
    k.k.type_ == T::KEY_TYPE.0 as u8 && (inum.inum == 0 || T::is_visible(inum, k))
}

fn is_whiteout(k: BkeySC<'_>) -> bool {
    k.k.type_ == c::bch_bkey_type::KEY_TYPE_hash_whiteout.0 as u8
}

/// Look up @key in hash table @T, in inode @inum as seen in @snapshot: as
/// bch2_hash_lookup_in_snapshot(). The key found, through @iter;
/// ENOENT_str_hash_lookup if there is none.
pub fn lookup_in_snapshot<'i, 't, T: HashTable>(
    t:         &'i TransAttempt<'_, 't>,
    iter:      &'i mut BtreeIter<'t>,
    hash_info: &c::bch_hash_info,
    inum:      c::subvol_inum,
    key:       &T::Key<'_>,
    flags:     BtreeIterFlags,
    snapshot:  u32,
) -> Result<BkeySC<'i>, BchError> {
    let flags = BtreeIterFlags::SLOTS | flags;
    *iter = BtreeIter::new(t, T::BTREE as u32,
                           spos(inum.inum, T::hash_key(hash_info, key), snapshot), flags);

    // The key, or a hole: the end of its probe sequence
    let found = iter.find_max_norestart(t, pos(inum.inum, u64::MAX), |_, k| Ok(
        if is_visible_key::<T>(inum, k) {
            T::matches(k, key)
        } else {
            !is_whiteout(k)
        }))?;

    match found {
        Some(k) if is_visible_key::<T>(inum, k) => Ok(k),
        _ => Err(t.fs().err(bch_errcode::BCH_ERR_ENOENT_str_hash_lookup)),
    }
}

/// Look up @key in hash table @T, in subvolume and inode @inum: as
/// bch2_hash_lookup(). The key found, through @iter;
/// ENOENT_str_hash_lookup if there is none.
pub fn lookup<'i, 't, T: HashTable>(
    t:         &'i TransAttempt<'_, 't>,
    iter:      &'i mut BtreeIter<'t>,
    hash_info: &c::bch_hash_info,
    inum:      c::subvol_inum,
    key:       &T::Key<'_>,
    flags:     BtreeIterFlags,
) -> Result<BkeySC<'i>, BchError> {
    let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;
    lookup_in_snapshot::<T>(t, iter, hash_info, inum, key, flags, snapshot)
}

/// Point @iter at the slot @key would be inserted at, in hash table @T in
/// subvolume and inode @inum - the first that holds no visible key: as
/// bch2_hash_hole(). ENOSPC_str_hash_create if the table is full.
pub fn hole<'t, T: HashTable>(
    t:         &TransAttempt<'_, 't>,
    iter:      &mut BtreeIter<'t>,
    hash_info: &c::bch_hash_info,
    inum:      c::subvol_inum,
    key:       &T::Key<'_>,
) -> Result<(), BchError> {
    let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;
    let flags = BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT;
    *iter = BtreeIter::new(t, T::BTREE as u32,
                           spos(inum.inum, T::hash_key(hash_info, key), snapshot), flags);

    match iter.find_max_norestart(t, pos(inum.inum, u64::MAX),
                                  |_, k| Ok(!is_visible_key::<T>(inum, k)))? {
        Some(_) => Ok(()),
        None    => Err(t.fs().err(bch_errcode::BCH_ERR_ENOSPC_str_hash_create)),
    }
}

/// Whether deleting the key at @start, in hash table @T, needs a whiteout
/// left in its place - a later key in the same probe sequence would be cut
/// off without one: as bch2_hash_needs_whiteout().
pub fn needs_whiteout<'t, T: HashTable>(
    t:         &TransAttempt<'_, 't>,
    hash_info: &c::bch_hash_info,
    start:     &BtreeIter<'t>,
) -> Result<bool, BchError> {
    let mut iter = start.copy();
    iter.set_flags(BtreeIterFlags::SLOTS);
    iter.advance();

    let ours = |k: BkeySC<'_>| k.k.type_ == T::KEY_TYPE.0 as u8;

    // A key that hashes at or before @start, or the end of the sequence
    let found = iter.find_max_norestart(t, SPOS_MAX, |_, k| Ok(
        if ours(k) {
            T::hash_bkey(hash_info, k) <= start.pos().offset
        } else {
            !is_whiteout(k)
        }))?;

    Ok(found.is_some_and(ours))
}

/// Insert @insert into hash table @T, in inode @inum as seen in @snapshot:
/// as bch2_hash_set_or_get_in_snapshot(). With STR_HASH_MUST_CREATE, a key
/// of the same name already there is returned, through @iter, and nothing
/// inserted; with STR_HASH_MUST_REPLACE, there must be one. None if it was
/// inserted (queued).
#[allow(clippy::too_many_arguments)]
pub fn set_or_get_in_snapshot<'i, 't, T: HashTable>(
    t:            &'i TransAttempt<'_, 't>,
    iter:         &'i mut BtreeIter<'t>,
    hash_info:    &c::bch_hash_info,
    inum:         c::subvol_inum,
    snapshot:     u32,
    insert:       &mut TransBkey<'_, 't>,
    iter_flags:   BtreeIterFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<Option<BkeySC<'i>>, BchError> {
    let fs = t.fs();
    let flags = iter_flags.bits() | update_flags.bits();
    let peek_flags = BtreeIterFlags::from_bits_retain(flags) |
        BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT;
    let must_create  = iter_flags.contains(BtreeIterFlags::STR_HASH_MUST_CREATE);
    let must_replace = iter_flags.contains(BtreeIterFlags::STR_HASH_MUST_REPLACE);

    let inode = insert.k().p.inode;
    *iter = BtreeIter::new(t, T::BTREE as u32,
                           spos(inode, T::hash_bkey(hash_info, BkeySC::from(insert.k_i())), snapshot),
                           peek_flags);

    // The first slot the key could go in, if there's no key of its name:
    let mut slot: Option<BtreeIter<'t>> = None;

    // A key of its name, or a hole: the end of its probe sequence
    let found = iter.find_max_norestart(t, pos(inode, u64::MAX), |iter, k| Ok(
        if is_visible_key::<T>(inum, k) {
            T::same_name(k, BkeySC::from(insert.k_i()))     // else a hash collision
        } else {
            if slot.is_none() && !must_replace {
                slot = Some(iter.copy());
            }
            !is_whiteout(k)
        }))?;

    let Some(k) = found else {
        return Err(fs.err(bch_errcode::BCH_ERR_ENOSPC_str_hash_create));
    };

    if is_visible_key::<T>(inum, k) {
        if must_create {
            // XXX polonius: returned here, @iter used below
            return Ok(Some(unsafe { polonius_key(k) }));
        }
    } else if must_replace {
        return Err(fs.err(bch_errcode::BCH_ERR_ENOENT_str_hash_set_must_replace));
    } else if let Some(slot) = slot.as_mut() {
        core::mem::swap(iter, slot);
    }

    insert.k_mut().p = iter.pos();
    // The iterator flags go along with the update's, as C passes them:
    t.update(iter, insert, UpdateTriggerFlags::from_bits_retain(flags))?;
    Ok(None)
}

/// Insert @insert into hash table @T, in inode @inum as seen in @snapshot:
/// as bch2_hash_set_in_snapshot(). EEXIST_str_hash_set if its name is taken.
pub fn set_in_snapshot<'t, T: HashTable>(
    t:            &TransAttempt<'_, 't>,
    hash_info:    &c::bch_hash_info,
    inum:         c::subvol_inum,
    snapshot:     u32,
    insert:       &mut TransBkey<'_, 't>,
    iter_flags:   BtreeIterFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    let mut iter = BtreeIter::uninit();
    match set_or_get_in_snapshot::<T>(t, &mut iter, hash_info, inum, snapshot, insert,
                                      iter_flags, update_flags)? {
        Some(_) => Err(t.fs().err(bch_errcode::BCH_ERR_EEXIST_str_hash_set)),
        None    => Ok(()),
    }
}

/// Insert @insert into hash table @T, in subvolume and inode @inum: as
/// bch2_hash_set(). EEXIST_str_hash_set if its name is taken.
pub fn set<'t, T: HashTable>(
    t:            &TransAttempt<'_, 't>,
    hash_info:    &c::bch_hash_info,
    inum:         c::subvol_inum,
    insert:       &mut TransBkey<'_, 't>,
    iter_flags:   BtreeIterFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    insert.k_mut().p.inode = inum.inum;

    let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;
    set_in_snapshot::<T>(t, hash_info, inum, snapshot, insert, iter_flags, update_flags)
}

/// Delete the key at @iter from hash table @T, leaving a whiteout if a later
/// key in the same probe sequence needs one: as bch2_hash_delete_at().
pub fn delete_at<'t, T: HashTable>(
    t:         &TransAttempt<'_, 't>,
    hash_info: &c::bch_hash_info,
    iter:      &mut BtreeIter<'t>,
    flags:     UpdateTriggerFlags,
) -> Result<(), BchError> {
    let type_ = if needs_whiteout::<T>(t, hash_info, iter)? {
        c::bch_bkey_type::KEY_TYPE_hash_whiteout
    } else {
        c::bch_bkey_type::KEY_TYPE_deleted
    };

    let k = t.bkey_alloc_init(0, type_.0 as u8, iter.pos())?;
    t.update(iter, &k, flags)
}

/// Delete @key from hash table @T, in subvolume and inode @inum: as
/// bch2_hash_delete(). ENOENT_str_hash_lookup if it isn't there.
pub fn delete<T: HashTable>(
    t:         &TransAttempt<'_, '_>,
    hash_info: &c::bch_hash_info,
    inum:      c::subvol_inum,
    key:       &T::Key<'_>,
) -> Result<(), BchError> {
    let mut iter = BtreeIter::uninit();
    lookup::<T>(t, &mut iter, hash_info, inum, key, BtreeIterFlags::INTENT)?;
    delete_at::<T>(t, hash_info, &mut iter, UpdateTriggerFlags::empty())
}

/// The hash type a new inode's dirents and xattrs get, from the str_hash
/// option: as bch2_str_hash_opt_to_type(c, c->opts.str_hash). The option is
/// read as a number, not taken as a bch_str_hash_opts - a value no variant has
/// isn't a Rust enum - and as in C, one that isn't an option is a bug.
pub fn new_inode_type(fs: &Fs) -> c::bch_str_hash_type {
    use c::bch_str_hash_opts::*;
    use c::bch_str_hash_type::*;

    match fs.opts().str_hash as u32 {
        o if o == BCH_STR_HASH_OPT_crc32c as u32 => BCH_STR_HASH_crc32c,
        o if o == BCH_STR_HASH_OPT_crc64 as u32  => BCH_STR_HASH_crc64,
        o if o == BCH_STR_HASH_OPT_siphash as u32 => {
            if fs.feature(c::bch_sb_feature::BCH_FEATURE_new_siphash) {
                BCH_STR_HASH_siphash
            } else {
                BCH_STR_HASH_siphash_old
            }
        }
        o => panic!("str_hash option {o} isn't one"),
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
    // `bcachefs dump --sanitize` scrubs dirent names in place without
    // updating their hash positions, so every dirent looks misplaced and
    // same-length names collide: on such an image the names are meaningless,
    // and nothing is checked or "repaired".
    if T::BTREE == c::btree_id::dirents && trans.fs().dirents_sanitized() {
        return Ok(());
    }

    if !needs_check::<T>(hash_info, k) {
        return Ok(());
    }

    let s = match s {
        Some(s) => Seen::Walk(s),
        None    => Seen::Lazy(None),
    };
    let mut r = Repair { s, hash_info, updated_before_k_pos };
    r.check_key::<T>(&trans.attempt_in_progress(), k)
}

/// The cheap test, before walking anything: whether @k is at the offset it
/// hashes to, and for a dirent, casefolded as its directory is.
fn needs_check<T: HashTable>(hash_info: &c::bch_hash_info, k: BkeySC<'_>) -> bool {
    if k.k.type_ != T::KEY_TYPE.0 as u8 {
        return false;
    }
    if T::hash_bkey(hash_info, k) != k.k.p.offset {
        return true;
    }
    k.as_dirent()
        .is_some_and(|d| (d.d_casefold() != 0) != !hash_info.cf_encoding.is_null())
}

// fsck: repairing hash table keys
//
// check_key() above is the dirents_sanitized guard and the cheap "is this
// key where it hashes to" test. Everything past that test - a key that isn't
// where its hash says, or has a duplicate ahead of it in its probe sequence -
// is here.
//
// Changes from the C:
//
// - hash_pick_winner() documented its result as "0 -> delete k1, 1 -> delete
//   k2", and str_hash_dup_entries() read it the other way round (swap, then
//   delete @k when nonzero) - so each repair deleted the key the winner
//   function meant to keep: of two duplicate dirents, the valid one rather
//   than the one pointing nowhere; of identical keys, the one in the right
//   place; across snapshots, the newer key instead of the older one in the
//   newer snapshot. DupResolution says which key goes, so the two can't
//   disagree.
//
// - repair_inode_hash_info() queued the inode write before asking - so the
//   message could print it - and a declined repair still wrote, when the
//   caller committed. It asks first now, with the new hash info in the
//   message.

/// Every version of an inode has the same hash seed and type: make
/// @bad_inode's match @snapshot_root's.
pub fn repair_inode_hash_info(
    t:             &TransAttempt<'_, '_>,
    bad_inode:     &mut c::bch_inode_unpacked,
    snapshot_root: &c::bch_inode_unpacked,
) -> Result<(), BchError> {
    let trans = t.trans();

    assert_eq!(bad_inode.bi_inum, snapshot_root.bi_inum);
    assert!(snapshot::is_ancestor(trans, bad_inode.bi_snapshot, snapshot_root.bi_snapshot));

    if !fsck_err!(trans, id::inode_snapshot_mismatch,
                  "inum {}: inode hash info in snapshots {}, {} mismatch\n{} {:x}\n{} {:x}\n\
                   setting the version in snapshot {} to the snapshot root's",
                  snapshot_root.bi_inum, bad_inode.bi_snapshot, snapshot_root.bi_snapshot,
                  StrHashType(bad_inode.str_hash()), bad_inode.bi_hash_seed,
                  StrHashType(snapshot_root.str_hash()), snapshot_root.bi_hash_seed,
                  bad_inode.bi_snapshot)? {
        return Ok(());
    }

    bad_inode.bi_hash_seed = snapshot_root.bi_hash_seed;
    bad_inode.set_str_hash(snapshot_root.str_hash());

    inode::fsck_write(t, bad_inode)?;
    t.commit_lazy(CommitFlags::NO_ENOSPC)
}

/// Whether dirent @d points at something that exists, as seen from its
/// snapshot: a subvolume, or an inode.
fn dirent_has_target(t: &TransAttempt<'_, '_>, d: BkeySC<'_>) -> Result<bool, BchError> {
    match Dirent::new(d).expect("a dirent").target() {
        DirentTarget::Subvol { child, .. } =>
            Ok(subvolume::get(t, child, false).found()?.is_some()),
        DirentTarget::Inode(inum) => {
            let mut iter = BtreeIter::new(t, c::btree_id::inodes,
                                          spos(0, inum, d.k.p.snapshot), BtreeIterFlags::empty());
            Ok(inode::bkey_is_inode(iter.peek_slot(t)?.expect("a slot always has a key").k))
        }
    }
}

/// Of two keys with the same name in one probe sequence - @k, being checked,
/// and @dup, ahead of it - which one to delete.
enum DupResolution {
    DeleteK,
    DeleteDup,
    /// Both are valid dirents: rename @k out of the way, then delete it.
    RenameK,
}

fn hash_pick_winner<T: HashTable>(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, dup: BkeySC<'_>)
    -> Result<DupResolution, BchError>
{
    use DupResolution::*;

    Ok(if k.val_bytes() == dup.val_bytes() {
        // The same key twice: delete the second, which is at the wrong offset
        DeleteK
    } else if k.k.p.snapshot != dup.k.p.snapshot {
        // Delete the older key from the newer snapshot
        if k.k.p.snapshot < dup.k.p.snapshot { DeleteDup } else { DeleteK }
    } else if T::BTREE != c::btree_id::dirents || !dirent_has_target(t, k)? {
        DeleteK
    } else if !dirent_has_target(t, dup)? {
        DeleteDup
    } else {
        RenameK
    })
}

/// Snapshot visibility at the key's position.
enum Seen<'r> {
    /// The walk's.
    Walk(&'r mut SnapshotsSeen),
    /// Out of a walk - from readdir - the snapshots that overwrote the key,
    /// worked out on first use.
    Lazy(Option<SnapshotsSeen>),
}

impl Seen<'_> {
    fn get(&mut self, t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos)
        -> Result<&mut SnapshotsSeen, BchError>
    {
        match self {
            Seen::Walk(s) => Ok(s),
            Seen::Lazy(s) => {
                if s.is_none() {
                    *s = Some(SnapshotsSeen::overwrites(t, btree, pos)?);
                }
                Ok(s.as_mut().expect("just computed"))
            }
        }
    }
}

/// What the repairs for one key share - the state C threaded through every
/// function.
struct Repair<'r> {
    s:                    Seen<'r>,
    hash_info:            &'r mut c::bch_hash_info,
    updated_before_k_pos: &'r mut bool,
}

impl Repair<'_> {
    /// @new, at @pos, may be a dirent that moved: point its inodes at it.
    fn update_backpointers<T: HashTable>(
        &mut self,
        t:   &TransAttempt<'_, '_>,
        pos: c::bpos,
        new: &c::bkey_i,
    ) -> Result<(), BchError> {
        let s = self.s.get(t, T::BTREE, pos)?;
        check::fsck_update_backpointers(t, s, new)
    }

    /// All versions of an inode must have the same hash seed and type: check
    /// the hash info in use is the snapshot root's, before repairing with it.
    fn check_hash_info(&mut self, t: &TransAttempt<'_, '_>, inum: u64) -> Result<(), BchError> {
        let trans = t.trans();
        let fs = trans.fs();
        let hash_info = &*self.hash_info;

        let snapshot_root = inode::find_oldest_snapshot(trans, inum, hash_info.inum_snapshot)?;
        let hash_root = hash_info_init_unchecked(fs, &snapshot_root);

        if hash_info.type_ == hash_root.type_ && hash_info.siphash_key == hash_root.siphash_key {
            return Ok(());
        }

        let mut bad_inode = inode::find_by_inum_snapshot(trans, inum, hash_info.inum_snapshot,
                                                         BtreeIterFlags::empty())?;
        assert!(*hash_info == hash_info_init_unchecked(fs, &bad_inode),
                "hash info in use doesn't match inode {inum}:{}", hash_info.inum_snapshot);

        repair_inode_hash_info(t, &mut bad_inode, &snapshot_root)
    }

    /// Give dirent @old a new name, "<name>.fsck_renamed-<n>", in the same
    /// directory and snapshot - to keep both of two valid dirents with the
    /// same name.
    fn rename_dirent(&mut self, t: &TransAttempt<'_, '_>, old: BkeySC<'_>)
        -> Result<(), BchError>
    {
        let fs = t.fs();
        let old_d = Dirent::new(old).expect("a dirent");
        let old_name = old_d.name();
        let dir = c::subvol_inum { subvol: 0, inum: old.k.p.inode };

        let mut new = dirent::alloc_max(t, old.k.p)?;
        dirent::copy_target(&mut new, old_d);

        // dirents already at each fsck_renamed-N name, gathered for diagnosis
        let mut collisions = Printbuf::new();

        for i in 0..1000 {
            let mut name = Printbuf::new();
            name.write_bytes(old_name.as_bytes());
            write!(name, ".fsck_renamed-{i}");

            new.k_mut().u64s = u8::MAX;
            dirent::init_name(fs, new.k_i_mut(), self.hash_info, name.as_os_str(), None)?;

            let mut iter = BtreeIter::uninit();
            match set_or_get_in_snapshot::<Dirents>(t, &mut iter, self.hash_info, dir,
                                                    old.k.p.snapshot, &mut new,
                                                    BtreeIterFlags::STR_HASH_MUST_CREATE,
                                                    UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)? {
                None => {
                    *self.updated_before_k_pos |= new.k().p < old.k.p;
                    return self.update_backpointers::<Dirents>(t, old.k.p, new.k_i());
                }
                // name taken - record a sample of the dirents that hold them
                Some(dup) if i < 10 => write!(collisions, "\n{}", dup.to_text(fs)),
                Some(_) => {}
            }
        }

        let mut buf = Printbuf::new();
        write!(buf, "couldn't rename dirent to resolve hash collision: all 1000 \"");
        buf.write_bytes(old_name.as_bytes());
        write!(buf, ".fsck_renamed-N\" names in dir inum {} snapshot {} are taken\n\
                     renaming:\n  {}\ncollided with:",
               { old.k.p.inode }, { old.k.p.snapshot }, old.to_text(fs));
        buf.indent_add(2);
        write!(buf, "{collisions}");
        bch_err!(fs, "{buf}");
        Err(fs.err(bch_errcode::BCH_ERR_EEXIST_str_hash_set).into())
    }

    /// @k and @dup, ahead of it in its probe sequence, have the same name:
    /// delete one, or rename @k out of the way.
    fn dup_entries<T: HashTable>(
        &mut self,
        t:   &TransAttempt<'_, '_>,
        k:   BkeySC<'_>,
        dup: BkeySC<'_>,
    ) -> Result<(), BchError> {
        let trans = t.trans();
        let fs = trans.fs();

        let res = hash_pick_winner::<T>(t, k, dup)?;
        let rename = matches!(res, DupResolution::RenameK);

        if !inode_fsck_err!(trans, k.k.p, id::hash_table_key_duplicate,
                            "duplicate hash table keys{}\n{}\n{}",
                            if rename { ", both point to valid inodes" } else { "" },
                            k.to_text(fs), dup.to_text(fs))? {
            return Ok(());
        }

        if rename {
            self.rename_dirent(t, k)?;
        }

        // @dup was found by a lookup from @k's snapshot, so if they're in
        // different snapshots @k is the descendant: delete the loser at @k's
        // snapshot. @dup losing, that whiteouts the ancestor's entry in @k's
        // view without touching the ancestor's own.
        let loser = if matches!(res, DupResolution::DeleteDup) { dup.k.p } else { k.k.p };

        let mut iter = BtreeIter::new(trans, T::BTREE,
                                      spos(loser.inode, loser.offset, k.k.p.snapshot),
                                      BtreeIterFlags::SLOTS);
        t.iter_traverse(&mut iter)?;
        delete_at::<T>(t, self.hash_info, &mut iter, UpdateTriggerFlags::empty())?;
        t.commit_lazy(CommitFlags::NO_ENOSPC)
    }

    /// Put @k in its proper location - unless a key of the same name is
    /// there, or with @dup, already known to be ahead of it: then resolve the
    /// duplicate.
    fn repair_key<T: HashTable>(
        &mut self,
        t:   &TransAttempt<'_, '_>,
        k:   BkeySC<'_>,
        dup: Option<BkeySC<'_>>,
    ) -> Result<(), BchError> {
        let trans = t.trans();
        let btree = T::BTREE;

        if let Some(dup) = dup {
            return self.dup_entries::<T>(t, k, dup);
        }

        let mut new = t.bkey_make_mut_noupdate(k)?;
        let mut iter = BtreeIter::uninit();
        let dir = c::subvol_inum { subvol: 0, inum: k.k.p.inode };

        if let Some(dup) = set_or_get_in_snapshot::<T>(t, &mut iter, self.hash_info, dir,
                                                       k.k.p.snapshot, &mut new,
                                                       BtreeIterFlags::STR_HASH_MUST_CREATE,
                                                       UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)? {
            return self.dup_entries::<T>(t, k, dup);
        }

        *self.updated_before_k_pos |= new.k().p < k.k.p;

        t.insert_snapshot_whiteouts(btree, k.k.p, new.k().p)?;

        let mut k_iter = BtreeIter::new(trans, btree, k.k.p, BtreeIterFlags::SLOTS);
        t.iter_traverse(&mut k_iter)?;
        delete_at::<T>(t, self.hash_info, &mut k_iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;

        self.update_backpointers::<T>(t, k.k.p, new.k_i())?;
        t.commit_lazy(CommitFlags::NO_ENOSPC)
    }

    /// @k can't be found by lookup - it's before the slot @hash it hashes
    /// to, or there's a hole in between: move it.
    fn bad_hash<T: HashTable>(&mut self, t: &TransAttempt<'_, '_>, k: BkeySC<'_>, hash: u64)
        -> Result<(), BchError>
    {
        let trans = t.trans();
        self.check_hash_info(t, k.k.p.inode)?;

        if fsck_err!(trans, id::hash_table_key_wrong_offset,
                     "hash table key at wrong offset: should be at {hash}\n{}",
                     k.to_text(trans.fs()))? {
            return self.repair_key::<T>(t, k, None);
        }
        Ok(())
    }

    /// A dirent's casefolding has to match its directory's: if it doesn't,
    /// recreate it with the directory's, and return str_hash_key_repaired - @k
    /// is gone.
    fn check_dirent(&mut self, t: &TransAttempt<'_, '_>, k: BkeySC<'_>) -> Result<(), BchError> {
        let trans = t.trans();
        let fs = trans.fs();
        let d = Dirent::new(k).expect("a dirent");

        if !fsck_err_on!(trans, (d.v().d_casefold() != 0) != !self.hash_info.cf_encoding.is_null(),
                         id::dirent_casefold_mismatch,
                         "dirent casefold does not match dir casefold\n{}", k.to_text(fs))? {
            return Ok(());
        }

        let (dir, target) = match d.target() {
            DirentTarget::Subvol { child, parent } =>
                (c::subvol_inum { subvol: parent as u64, inum: 0 }, child as u64),
            DirentTarget::Inode(inum) =>
                (c::subvol_inum { subvol: 0, inum: 0 }, inum),
        };

        let mut new = dirent::create_key(t, self.hash_info, dir, d.d_type(),
                                         d.name(), None, target)?;
        new.k_mut().p.inode    = k.k.p.inode;
        new.k_mut().p.snapshot = k.k.p.snapshot;

        let mut iter = BtreeIter::new(trans, c::btree_id::dirents, k.k.p, BtreeIterFlags::SLOTS);
        delete_at::<Dirents>(t, self.hash_info, &mut iter,
                             UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;

        self.repair_key::<Dirents>(t, BkeySC::from(new.k_i()), None)?;
        Err(fs.err(bch_errcode::BCH_ERR_str_hash_key_repaired).into())
    }

    /// The rest of check_key(), for a key not at the offset it hashes to:
    /// walk its probe sequence for an empty slot or a duplicate.
    fn check_key<T: HashTable>(&mut self, t: &TransAttempt<'_, '_>, hash_k: BkeySC<'_>)
        -> Result<(), BchError>
    {
        let trans = t.trans();

        let hash = T::hash_bkey(self.hash_info, hash_k);
        if hash_k.k.p.offset < hash {
            return self.bad_hash::<T>(t, hash_k, hash);
        }

        // Probing from the slot it hashes to: itself, a duplicate - a key with
        // the same name - or a hole, which lookups would stop at
        let mut iter = BtreeIter::new(trans, T::BTREE as u32,
                                      spos(hash_k.k.p.inode, hash, hash_k.k.p.snapshot),
                                      BtreeIterFlags::SLOTS);
        let found = iter.find_max_norestart(t, SPOS_MAX, |_, k| Ok(
            k.k.p == hash_k.k.p ||
            (k.k.type_ == T::KEY_TYPE.0 as u8 && T::same_name(k, hash_k)) ||
            k.is_deleted()))?;

        match found {
            Some(k) if k.k.p == hash_k.k.p => {}
            Some(hole) if hole.is_deleted() => return self.bad_hash::<T>(t, hash_k, hash),
            Some(dup) => {
                // A repair of the hash info commits, and restarts: past this,
                // nothing's been committed and @dup is still good
                self.check_hash_info(t, hash_k.k.p.inode)?;
                self.repair_key::<T>(t, hash_k, Some(dup))?;
            }
            None => {}
        }

        if hash_k.key_type() == c::bch_bkey_type::KEY_TYPE_dirent {
            self.check_dirent(t, hash_k)?;
        }
        Ok(())
    }
}
