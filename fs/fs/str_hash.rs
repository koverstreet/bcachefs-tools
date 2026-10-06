// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::{spos, BkeySC};
use crate::btree::iter::{
    bkey_s_c_to_result, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
    TransRet, UpdateTriggerFlags,
};
use crate::c;
use crate::check::{self, SnapshotsSeen};
use crate::dirent::{self, DirentTarget, Dirents};
use crate::errcode::{bch_errcode, ret_to_result_void as ret_to_result, BchError, Found};
use crate::fs::Fs;
use crate::init::error::id;
use crate::inode;
use crate::printbuf_to_formatter;
use crate::snapshots::{snapshot, subvolume};
use crate::util::Printbuf;
use crate::xattr::Xattrs;
use crate::{bch_err, fsck_err, fsck_err_on, inode_fsck_err};
use core::ffi::{c_int, c_void};
use core::fmt;
use core::ops::ControlFlow;

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
    unsafe {
        let k = c::bch2_hash_set_or_get_in_snapshot(trans.raw(), iter.raw_mut(), *T::desc(),
                                                    hash_info, inum, snapshot, insert,
                                                    c::btree_iter_update_trigger_flags(flags));
        bkey_s_c_to_result(k)
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
        let k = c::bch2_hash_lookup_in_snapshot(trans.raw(), iter.raw_mut(), *T::desc(), hash_info,
                                                inum, key as *const T::Key as *const c_void,
                                                c::btree_iter_update_trigger_flags(flags.bits()),
                                                snapshot);
        bkey_s_c_to_result(k)?
    };
    Ok(k.expect("a hash lookup returns a key or an error"))
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
    ret_to_result(unsafe {
        c::bch2_str_hash_check_key(trans.raw(),
                                   s.map_or(core::ptr::null_mut(), |s| s.as_opaque()),
                                   T::desc(), hash_info, k.to_raw(),
                                   updated_before_k_pos)
    })
}

// fsck: repairing hash table keys
//
// check_key() above is the C inline bch2_str_hash_check_key(): the
// dirents_sanitized guard and the cheap "is this key where it hashes to"
// test, in C because readdir calls it too. Everything past that test - a key
// that isn't where its hash says, or has a duplicate ahead of it in its probe
// sequence - is here, entered through __bch2_str_hash_check_key(), exported
// for that inline.
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
pub fn repair_inode_hash_info<'a, 't>(
    t:             TransAttempt<'a, 't>,
    bad_inode:     &mut c::bch_inode_unpacked,
    snapshot_root: &c::bch_inode_unpacked,
) -> TransRet<'a, 't> {
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
        return Ok(t);
    }

    bad_inode.bi_hash_seed = snapshot_root.bi_hash_seed;
    bad_inode.set_str_hash(snapshot_root.str_hash());

    let t = inode::fsck_write(t, bad_inode)?;
    t.commit_lazy(CommitFlags::NO_ENOSPC)
}

/// Whether dirent @d points at something that exists, as seen from its
/// snapshot: a subvolume, or an inode.
fn dirent_has_target(trans: &BtreeTrans<'_>, d: BkeySC<'_>) -> Result<bool, BchError> {
    match d.as_dirent().expect("a dirent").target() {
        DirentTarget::Subvol { child, .. } =>
            Ok(subvolume::get(trans, child, false).found()?.is_some()),
        DirentTarget::Inode(inum) => {
            let mut iter = BtreeIter::new(trans, c::btree_id::inodes,
                                          spos(0, inum, d.k.p.snapshot), BtreeIterFlags::empty());
            Ok(inode::bkey_is_inode(iter.peek_slot()?.expect("a slot always has a key").k))
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

fn hash_pick_winner<T: HashTable>(trans: &BtreeTrans<'_>, k: BkeySC<'_>, dup: BkeySC<'_>)
    -> Result<DupResolution, BchError>
{
    use DupResolution::*;

    Ok(if k.val_bytes() == dup.val_bytes() {
        // The same key twice: delete the second, which is at the wrong offset
        DeleteK
    } else if k.k.p.snapshot != dup.k.p.snapshot {
        // Delete the older key from the newer snapshot
        if k.k.p.snapshot < dup.k.p.snapshot { DeleteDup } else { DeleteK }
    } else if T::desc().btree_id != c::btree_id::dirents || !dirent_has_target(trans, k)? {
        DeleteK
    } else if !dirent_has_target(trans, dup)? {
        DeleteDup
    } else {
        RenameK
    })
}

/// Probing @hash_k's hash slots up to it, what came first.
enum ProbeFound {
    Itself,
    /// A key with the same name, here.
    Dup(c::bpos),
    /// An empty slot: lookups can't find @hash_k.
    Hole,
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
    fn get(&mut self, trans: &BtreeTrans<'_>, btree: c::btree_id, pos: c::bpos)
        -> Result<&mut SnapshotsSeen, BchError>
    {
        match self {
            Seen::Walk(s) => Ok(s),
            Seen::Lazy(s) => {
                if s.is_none() {
                    *s = Some(SnapshotsSeen::overwrites(trans, btree, pos)?);
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
    fn update_backpointers<'a, 't, T: HashTable>(
        &mut self,
        t:   TransAttempt<'a, 't>,
        pos: c::bpos,
        new: &c::bkey_i,
    ) -> TransRet<'a, 't> {
        let s = self.s.get(t.trans(), T::desc().btree_id, pos)?;
        check::fsck_update_backpointers(t, s, new)
    }

    /// All versions of an inode must have the same hash seed and type: check
    /// the hash info in use is the snapshot root's, before repairing with it.
    fn check_hash_info<'a, 't>(&mut self, t: TransAttempt<'a, 't>, inum: u64) -> TransRet<'a, 't> {
        let trans = t.trans();
        let fs = trans.fs();
        let hash_info = &*self.hash_info;

        let snapshot_root = inode::find_oldest_snapshot(trans, inum, hash_info.inum_snapshot)?;
        let hash_root = hash_info_init_unchecked(fs, &snapshot_root);

        if hash_info.type_ == hash_root.type_ && hash_info.siphash_key == hash_root.siphash_key {
            return Ok(t);
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
    fn rename_dirent<'a, 't>(&mut self, t: TransAttempt<'a, 't>, old: BkeySC<'_>)
        -> TransRet<'a, 't>
    {
        let trans = t.trans();
        let fs = trans.fs();
        let old_name = dirent::name(old);
        let dir = c::subvol_inum { subvol: 0, inum: old.k.p.inode };

        let mut new = dirent::alloc_max(&t, old.k.p)?;
        dirent::copy_target(&mut new, old);

        // dirents already at each fsck_renamed-N name, gathered for diagnosis
        let mut collisions = Printbuf::new();

        for i in 0..1000 {
            let mut name = Printbuf::new();
            name.write_bytes(old_name);
            write!(name, ".fsck_renamed-{i}");

            new.k_mut().u64s = u8::MAX;
            dirent::init_name(fs, &mut new, self.hash_info, name.as_bytes())?;

            let mut iter = BtreeIter::uninit();
            match set_or_get_in_snapshot::<Dirents>(trans, &mut iter, self.hash_info, dir,
                                                    old.k.p.snapshot, new.k_i_mut(),
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
        buf.write_bytes(old_name);
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
    fn dup_entries<'a, 't, T: HashTable>(
        &mut self,
        t:   TransAttempt<'a, 't>,
        k:   BkeySC<'_>,
        dup: BkeySC<'_>,
    ) -> TransRet<'a, 't> {
        let trans = t.trans();
        let fs = trans.fs();
        let mut t = t;

        let res = hash_pick_winner::<T>(trans, k, dup)?;
        let rename = matches!(res, DupResolution::RenameK);

        if !inode_fsck_err!(trans, k.k.p, id::hash_table_key_duplicate,
                            "duplicate hash table keys{}\n{}\n{}",
                            if rename { ", both point to valid inodes" } else { "" },
                            k.to_text(fs), dup.to_text(fs))? {
            return Ok(t);
        }

        if rename {
            t = self.rename_dirent(t, k)?;
        }

        // @dup was found by a lookup from @k's snapshot, so if they're in
        // different snapshots @k is the descendant: delete the loser at @k's
        // snapshot. @dup losing, that whiteouts the ancestor's entry in @k's
        // view without touching the ancestor's own.
        let loser = if matches!(res, DupResolution::DeleteDup) { dup.k.p } else { k.k.p };

        let mut iter = BtreeIter::new(trans, T::desc().btree_id,
                                      spos(loser.inode, loser.offset, k.k.p.snapshot),
                                      BtreeIterFlags::SLOTS);
        t = t.iter_traverse(&mut iter)?;
        delete_at::<T>(trans, self.hash_info, &mut iter, UpdateTriggerFlags::empty())?;
        t.commit_lazy(CommitFlags::NO_ENOSPC)
    }

    /// Put @k in its proper location - unless a key of the same name is
    /// there, or with @dup, already known to be ahead of it: then resolve the
    /// duplicate.
    fn repair_key<'a, 't, T: HashTable>(
        &mut self,
        t:   TransAttempt<'a, 't>,
        k:   BkeySC<'_>,
        dup: Option<BkeySC<'_>>,
    ) -> TransRet<'a, 't> {
        let trans = t.trans();
        let btree = T::desc().btree_id;

        if let Some(dup) = dup {
            return self.dup_entries::<T>(t, k, dup);
        }

        let mut new = t.bkey_make_mut_noupdate(k)?;
        let mut iter = BtreeIter::uninit();
        let dir = c::subvol_inum { subvol: 0, inum: k.k.p.inode };

        if let Some(dup) = set_or_get_in_snapshot::<T>(trans, &mut iter, self.hash_info, dir,
                                                       k.k.p.snapshot, new.k_i_mut(),
                                                       BtreeIterFlags::STR_HASH_MUST_CREATE,
                                                       UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)? {
            return self.dup_entries::<T>(t, k, dup);
        }

        *self.updated_before_k_pos |= new.k().p < k.k.p;

        let mut t = t.insert_snapshot_whiteouts(btree, k.k.p, new.k().p)?;

        let mut k_iter = BtreeIter::new(trans, btree, k.k.p, BtreeIterFlags::SLOTS);
        t = t.iter_traverse(&mut k_iter)?;
        delete_at::<T>(trans, self.hash_info, &mut k_iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;

        t = self.update_backpointers::<T>(t, k.k.p, new.k_i())?;
        t.commit_lazy(CommitFlags::NO_ENOSPC)
    }

    /// @k can't be found by lookup - it's before the slot @hash it hashes
    /// to, or there's a hole in between: move it.
    fn bad_hash<'a, 't, T: HashTable>(&mut self, t: TransAttempt<'a, 't>, k: BkeySC<'_>, hash: u64)
        -> TransRet<'a, 't>
    {
        let trans = t.trans();
        let t = self.check_hash_info(t, k.k.p.inode)?;

        if fsck_err!(trans, id::hash_table_key_wrong_offset,
                     "hash table key at wrong offset: should be at {hash}\n{}",
                     k.to_text(trans.fs()))? {
            return self.repair_key::<T>(t, k, None);
        }
        Ok(t)
    }

    /// A dirent's casefolding has to match its directory's: if it doesn't,
    /// recreate it with the directory's, and return str_hash_key_repaired - @k
    /// is gone.
    fn check_dirent<'a, 't>(&mut self, t: TransAttempt<'a, 't>, k: BkeySC<'_>) -> TransRet<'a, 't> {
        let trans = t.trans();
        let fs = trans.fs();
        let d = k.as_dirent().expect("a dirent");

        if !fsck_err_on!(trans, (d.d_casefold() != 0) != !self.hash_info.cf_encoding.is_null(),
                         id::dirent_casefold_mismatch,
                         "dirent casefold does not match dir casefold\n{}", k.to_text(fs))? {
            return Ok(t);
        }

        let (dir, target) = match d.target() {
            DirentTarget::Subvol { child, parent } =>
                (c::subvol_inum { subvol: parent as u64, inum: 0 }, child as u64),
            DirentTarget::Inode(inum) =>
                (c::subvol_inum { subvol: 0, inum: 0 }, inum),
        };

        let mut new = dirent::create_key(&t, self.hash_info, dir, d.d_type(), dirent::name(k),
                                         target)?;
        new.k_mut().p.inode    = k.k.p.inode;
        new.k_mut().p.snapshot = k.k.p.snapshot;

        let mut iter = BtreeIter::new(trans, c::btree_id::dirents, k.k.p, BtreeIterFlags::SLOTS);
        delete_at::<Dirents>(trans, self.hash_info, &mut iter,
                             UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;

        self.repair_key::<Dirents>(t, BkeySC::from(new.k_i()), None)?;
        Err(fs.err(bch_errcode::BCH_ERR_str_hash_key_repaired).into())
    }

    /// The rest of check_key(), for a key not at the offset it hashes to:
    /// walk its probe sequence for an empty slot or a duplicate.
    fn check_key<'a, 't, T: HashTable>(&mut self, t: TransAttempt<'a, 't>, hash_k: BkeySC<'_>)
        -> TransRet<'a, 't>
    {
        let trans = t.trans();
        let desc = T::desc();
        let mut t = t;

        let hash = T::hash_bkey(self.hash_info, hash_k);
        if hash_k.k.p.offset < hash {
            return self.bad_hash::<T>(t, hash_k, hash);
        }

        let mut found = ProbeFound::Itself;
        let mut iter = BtreeIter::new(trans, desc.btree_id,
                                      spos(hash_k.k.p.inode, hash, hash_k.k.p.snapshot),
                                      BtreeIterFlags::SLOTS);
        iter.for_each_norestart(|_, k| {
            if k.k.p == hash_k.k.p {
                return Ok(ControlFlow::Break(()));
            }
            if k.k.type_ == desc.key_type && T::same_name(k, hash_k) {
                found = ProbeFound::Dup(k.k.p);
                return Ok(ControlFlow::Break(()));
            }
            if k.is_deleted() {
                found = ProbeFound::Hole;
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        })?;

        match found {
            ProbeFound::Itself => {}
            ProbeFound::Hole => return self.bad_hash::<T>(t, hash_k, hash),
            ProbeFound::Dup(pos) => {
                t = self.check_hash_info(t, hash_k.k.p.inode)?;

                iter.set_pos(pos);
                let dup = iter.peek_slot()?.expect("a slot always has a key");
                t = self.repair_key::<T>(t, hash_k, Some(dup))?;
            }
        }

        if hash_k.key_type() == c::bch_bkey_type::KEY_TYPE_dirent {
            t = self.check_dirent(t, hash_k)?;
        }
        Ok(t)
    }
}

/// For C's bch2_str_hash_check_key() - readdir's, and check_key() above -
/// once its quick test has found @hash_k out of place.
///
/// # Safety
/// The arguments are the C function's: @trans live with an attempt in
/// progress, @s NULL or a struct snapshots_seen nothing else is using, and
/// the rest valid for the call.
#[no_mangle]
pub unsafe extern "C" fn __bch2_str_hash_check_key(
    trans:                *mut c::btree_trans,
    s:                    Option<&mut SnapshotsSeen>,
    desc:                 &c::bch_hash_desc,
    hash_info:            &mut c::bch_hash_info,
    hash_k:               c::bkey_s_c,
    updated_before_k_pos: &mut bool,
) -> c_int {
    let fs = unsafe { Fs::borrow_raw((*trans).c) };
    let trans = unsafe { BtreeTrans::borrow_raw(&fs, trans) };
    let t = trans.attempt_in_progress();
    let k = BkeySC::from(&hash_k);

    let s = match s {
        Some(s) => Seen::Walk(s),
        None    => Seen::Lazy(None),
    };
    let mut r = Repair { s, hash_info, updated_before_k_pos };

    let ret = if core::ptr::eq(desc, Dirents::desc()) {
        r.check_key::<Dirents>(t, k)
    } else if core::ptr::eq(desc, Xattrs::desc()) {
        r.check_key::<Xattrs>(t, k)
    } else {
        unreachable!("no hash table in btree {}", desc.btree_id as u32)
    };

    match ret {
        Ok(_)  => 0,
        Err(e) => -BchError::from(e).raw(),
    }
}
