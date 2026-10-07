// SPDX-License-Identifier: GPL-2.0

//! Dirents (fs/dirent.c): directory entries - a hash table per directory in
//! the dirents btree, keyed by the hash of the name - and the operations on
//! them: creating, looking up, iterating.
//!
//! The value itself - its target, its names, and what may be trusted of
//! them - is dirent_format.rs. A dirent's consistency with the inode it names
//! is namei's (namei.rs, as in C).

#[path = "dirent_format.rs"]
mod format;

pub use format::{d_type_str, set_field, Dirent, DirentName, DirentTarget, RawNames};
use format::{D_NAME_OFFSET, D_NAMES_OFFSET};

use crate::btree::bkey::{pos, spos, BkeyS, BkeySC, BKEY_U64S};
use crate::btree::iter::{
    lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt, TransBkey,
    UpdateTriggerFlags,
};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::inode;
use crate::snapshots::subvolume;
use crate::str_hash::{self, HashTable};
use crate::util::os_str::{qstr, qstr_name, OsStr, OsStrExt};
use core::ffi::c_int;
use core::ops::ControlFlow;

// ── The hash table ───────────────────────────────────────────────────────

/// The dirents btree, as a hash table.
pub struct Dirents;

/// Keys are looked up by the name they're hashed by - casefolded, in a
/// casefolded directory.
impl HashTable for Dirents {
    type Key<'k> = OsStr;

    const BTREE:    c::btree_id      = c::btree_id::dirents;
    const KEY_TYPE: c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_dirent;

    fn hash_key(info: &c::bch_hash_info, key: &OsStr) -> u64 {
        hash(info, key)
    }

    fn hash_bkey(info: &c::bch_hash_info, k: BkeySC<'_>) -> u64 {
        hash(info, Dirent::new(k).expect("a dirent").lookup_name())
    }

    fn matches(k: BkeySC<'_>, key: &OsStr) -> bool {
        Dirent::new(k).expect("a dirent").lookup_name() == key
    }

    fn same_name(a: BkeySC<'_>, b: BkeySC<'_>) -> bool {
        Dirent::new(a).expect("a dirent").lookup_name() ==
            Dirent::new(b).expect("a dirent").lookup_name()
    }

    /// A subvolume's dirent lists only in its parent subvolume.
    fn is_visible(inum: c::subvol_inum, k: BkeySC<'_>) -> bool {
        match Dirent::new(k).expect("a dirent").target() {
            DirentTarget::Subvol { parent, .. } => u64::from(parent) == inum.subvol,
            DirentTarget::Inode(_)              => true,
        }
    }
}

/// @name's hash under @info, its offset in its directory: as
/// bch2_dirent_hash(). Offsets 0 and 1 are kept for . and .., which aren't
/// stored.
pub fn hash(info: &c::bch_hash_info, name: &OsStr) -> u64 {
    str_hash::hash_parts(info, &[name.as_bytes()], true).max(2)
}

// ── Making dirents ───────────────────────────────────────────────────────

/// A dirent key at @pos with room for any name, to be filled in with
/// copy_target() and init_name().
pub fn alloc_max<'a, 't>(t: &TransAttempt<'a, 't>, pos: c::bpos)
    -> Result<TransBkey<'a, 't>, BchError>
{
    t.bkey_alloc_init(u8::MAX as usize - BKEY_U64S,
                      c::bch_bkey_type::KEY_TYPE_dirent.0 as u8, pos)
}

/// Point @dst, a dirent, at what @src points at: as dirent_copy_target().
pub fn copy_target(dst: &mut TransBkey<'_, '_>, src: Dirent<'_>) {
    let d = dst.k_i_mut().as_mut_dirent().expect("a dirent");
    d.__bindgen_anon_1 = src.v().__bindgen_anon_1;
    d.set_d_type(src.d_type());
}

/// Give @k, a dirent sized to the room it has, the name @name, and size it to
/// fit: as bch2_dirent_init_name(). In a casefolded directory - per
/// @hash_info - the casefolded name follows it: @cf_name if the caller has
/// it, otherwise made here.
pub fn init_name(
    fs:        &Fs,
    k:         &mut c::bkey_i,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
    cf_name:   Option<&OsStr>,
) -> Result<(), BchError> {
    let name = name.as_bytes();
    let cf_name = cf_name.map(OsStrExt::as_bytes);

    if name.len() > c::BCH_NAME_MAX as usize {
        return Err(BchError::from(c::ENAMETOOLONG));
    }

    let casefold = !hash_info.cf_encoding.is_null();
    debug_assert!(casefold || cf_name.is_none(), "a casefolded name for a dirent that isn't");
    k.as_mut_dirent().expect("a dirent").set_d_casefold(casefold as u8);

    let end = if !casefold {
        let mut s = BkeyS::from(&mut *k);
        let v = s.val_bytes_mut();
        v[D_NAME_OFFSET..][..name.len()].copy_from_slice(name);
        D_NAME_OFFSET + name.len()
    } else {
        fs.casefold_enabled()?;
        casefold_name(k, hash_info, name, cf_name)?
    };

    // NUL padding to the end of the u64: what the name's length is read from.
    let val_u64s = end.div_ceil(8);
    BkeyS::from(&mut *k).val_bytes_mut()[end..val_u64s * 8].fill(0);

    assert!(BKEY_U64S + val_u64s <= k.k.u64s as usize,
            "dirent name ({} bytes) overruns its key ({} u64s)", end, k.k.u64s);
    k.k.u64s = (BKEY_U64S + val_u64s) as u8;
    Ok(())
}

/// The casefolded half of init_name(): both names into the name block, and
/// their lengths. Where the names end.
#[cfg(CONFIG_UNICODE)]
fn casefold_name(
    k:         &mut c::bkey_i,
    hash_info: &c::bch_hash_info,
    name:      &[u8],
    cf_name:   Option<&[u8]>,
) -> Result<usize, BchError> {
    let mut s = BkeyS::from(&mut *k);
    let v = s.val_bytes_mut();
    let (names, out) = v[D_NAMES_OFFSET..].split_at_mut(name.len());
    names.copy_from_slice(name);

    let cf_len = match cf_name {
        Some(cf) => {
            out[..cf.len()].copy_from_slice(cf);
            cf.len()
        }
        None => utf8_casefold(hash_info, OsStr::from_bytes(name), out)?,
    };

    let cf_block = unsafe {
        k.as_mut_dirent().expect("a dirent").__bindgen_anon_2.d_cf_name_block.as_mut()
    };
    cf_block.d_name_len    = (name.len() as u16).to_le();
    cf_block.d_cf_name_len = (cf_len as u16).to_le();

    Ok(D_NAMES_OFFSET + name.len() + cf_len)
}

/// Without CONFIG_UNICODE, bch2_fs_casefold_enabled() has refused already.
#[cfg(not(CONFIG_UNICODE))]
fn casefold_name(
    _k:         &mut c::bkey_i,
    _hash_info: &c::bch_hash_info,
    _name:      &[u8],
    _cf_name:   Option<&[u8]>,
) -> Result<usize, BchError> {
    unreachable!("casefolding without CONFIG_UNICODE")
}

/// A new dirent in directory @dir named @name, of type @d_type, pointing at
/// @target - an inode, or for DT_SUBVOL a subvolume - its casefolded name
/// @cf_name if the caller has it: as bch2_dirent_create_key(). Its position
/// is for the caller to fill in.
pub fn create_key<'a, 't>(
    t:         &TransAttempt<'a, 't>,
    hash_info: &c::bch_hash_info,
    dir:       c::subvol_inum,
    d_type:    u8,
    name:      &OsStr,
    cf_name:   Option<&OsStr>,
    target:    u64,
) -> Result<TransBkey<'a, 't>, BchError> {
    let mut k = alloc_max(t, pos(dir.inum, 0))?;

    let d = k.k_i_mut().as_mut_dirent().expect("a dirent");
    d.set_d_type(d_type);
    if d_type as u32 != c::DT_SUBVOL {
        d.__bindgen_anon_1.d_inum = target.to_le();
    } else {
        d.__bindgen_anon_1.__bindgen_anon_1 = c::bch_dirent__bindgen_ty_1__bindgen_ty_1 {
            d_child_subvol:  (target as u32).to_le(),
            d_parent_subvol: (dir.subvol as u32).to_le(),
        };
    }

    init_name(t.fs(), k.k_i_mut(), hash_info, name, cf_name)?;
    Ok(k)
}

/// Create dirent @name in directory @dir, in subvolume @subvol at @snapshot,
/// pointing at @target: as bch2_dirent_create_snapshot(). Where it went is
/// returned in @dir_offset.
#[allow(clippy::too_many_arguments)]
pub fn create_snapshot(
    t:            &TransAttempt<'_, '_>,
    subvol:       u32,
    snapshot:     u32,
    dir:          &c::bch_inode_unpacked,
    d_type:       u8,
    name:         &OsStr,
    target:       DirentTarget,
    dir_offset:   &mut u64,
    iter_flags:   BtreeIterFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    let dir_inum = c::subvol_inum { subvol: subvol as u64, inum: dir.bi_inum };
    let hash_info = str_hash::hash_info_init(t.fs(), dir)?;

    // A subvolume dirent's parent is @subvol:
    let target = match target {
        DirentTarget::Subvol { child, .. } => child as u64,
        DirentTarget::Inode(inum)          => inum,
    };

    let mut k = create_key(t, &hash_info, dir_inum, d_type, name, None, target)?;
    let ret = str_hash::set_in_snapshot::<Dirents>(t, &hash_info, dir_inum, snapshot, &mut k,
                                                   iter_flags, update_flags);
    *dir_offset = k.k().p.offset;
    ret
}

/// For C's create and link: bch2_dirent_create_snapshot().
///
/// # Safety
/// The arguments are the C function's, valid for the call; @trans has an
/// attempt in progress.
#[no_mangle]
pub unsafe extern "C" fn bch2_dirent_create_snapshot(
    trans:      *mut c::btree_trans,
    dir_subvol: u32,
    snapshot:   u32,
    dir_u:      *mut c::bch_inode_unpacked,
    d_type:     u8,
    name:       *const c::qstr,
    dst_inum:   u64,
    dir_offset: *mut u64,
    flags:      c::btree_iter_update_trigger_flags,
) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };

    let target = if d_type as u32 == c::DT_SUBVOL {
        DirentTarget::Subvol { child: dst_inum as u32, parent: dir_subvol }
    } else {
        DirentTarget::Inode(dst_inum)
    };

    let ret = create_snapshot(&trans.attempt_in_progress(), dir_subvol, snapshot,
                              unsafe { &*dir_u }, d_type, unsafe { qstr_name(&*name) }, target,
                              unsafe { &mut *dir_offset },
                              BtreeIterFlags::from_bits_retain(flags.0),
                              UpdateTriggerFlags::from_bits_retain(flags.0));
    ret_to_c(ret)
}

/// Create dirent @name in directory @dir, pointing at @target - for
/// DT_SUBVOL, a subvolume: as bch2_dirent_create(). Where it went is returned
/// in @dir_offset.
#[allow(clippy::too_many_arguments)]
pub fn create(
    t:            &TransAttempt<'_, '_>,
    dir:          c::subvol_inum,
    dir_u:        &c::bch_inode_unpacked,
    d_type:       u8,
    name:         &OsStr,
    target:       u64,
    dir_offset:   &mut u64,
    iter_flags:   BtreeIterFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    let hash_info = str_hash::hash_info_init(t.fs(), dir_u)?;

    let mut k = create_key(t, &hash_info, dir, d_type, name, None, target)?;
    let ret = str_hash::set::<Dirents>(t, &hash_info, dir, &mut k, iter_flags, update_flags);
    *dir_offset = k.k().p.offset;
    ret
}

/// For C's create: bch2_dirent_create().
///
/// # Safety
/// The arguments are the C function's, valid for the call; @trans has an
/// attempt in progress.
#[no_mangle]
pub unsafe extern "C" fn bch2_dirent_create(
    trans:      *mut c::btree_trans,
    dir:        c::subvol_inum,
    dir_u:      *mut c::bch_inode_unpacked,
    d_type:     u8,
    name:       *const c::qstr,
    dst_inum:   u64,
    dir_offset: *mut u64,
    flags:      c::btree_iter_update_trigger_flags,
) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };

    let ret = create(&trans.attempt_in_progress(), dir, unsafe { &*dir_u }, d_type,
                     unsafe { qstr_name(&*name) }, dst_inum, unsafe { &mut *dir_offset },
                     BtreeIterFlags::from_bits_retain(flags.0),
                     UpdateTriggerFlags::from_bits_retain(flags.0));
    ret_to_c(ret)
}

// ── Casefolding ──────────────────────────────────────────────────────────

/// @name casefolded under @info's encoding, into @out: how long it is - as
/// utf8_casefold().
#[cfg(CONFIG_UNICODE)]
fn utf8_casefold(info: &c::bch_hash_info, name: &OsStr, out: &mut [u8]) -> Result<usize, BchError> {
    let ret = unsafe { c::bch2_utf8_casefold(info, &qstr(name), out.as_mut_ptr(), out.len()) };
    ret_to_result(ret.min(0))?;
    Ok(ret as usize)
}

/// @name casefolded under @info's encoding, in transaction memory: as
/// bch2_casefold().
#[cfg(CONFIG_UNICODE)]
pub fn casefold<'a>(
    t:    &TransAttempt<'a, '_>,
    info: &c::bch_hash_info,
    name: &OsStr,
) -> Result<&'a OsStr, BchError> {
    t.fs().casefold_enabled()?;

    let buf = t.kmalloc(c::BCH_NAME_MAX as usize + 1)?;
    let len = utf8_casefold(info, name, buf)?;
    Ok(OsStr::from_bytes(&buf[..len]))
}

#[cfg(not(CONFIG_UNICODE))]
pub fn casefold<'a>(
    t:     &TransAttempt<'a, '_>,
    _info: &c::bch_hash_info,
    _name: &OsStr,
) -> Result<&'a OsStr, BchError> {
    Err(t.fs().err(bch_errcode::BCH_ERR_no_casefolding_without_utf8))
}

/// The name @name is hashed and looked up by in a directory with @info -
/// casefolded, if the directory is: as bch2_maybe_casefold().
pub fn maybe_casefold<'a>(
    t:    &TransAttempt<'a, '_>,
    info: &c::bch_hash_info,
    name: &'a OsStr,
) -> Result<&'a OsStr, BchError> {
    if info.cf_encoding.is_null() {
        Ok(name)
    } else {
        casefold(t, info, name)
    }
}

// ── Reading dirents ──────────────────────────────────────────────────────

/// For C: @d's name, as bch2_dirent_get_name().
///
/// # Safety
/// @d is a dirent from the btree - valid - as C's callers' are.
#[no_mangle]
pub unsafe extern "C" fn bch2_dirent_get_name(d: c::bkey_s_c_dirent) -> c::qstr {
    let k = c::bkey_s_c::from(d);
    qstr(Dirent::new(BkeySC::from(&k)).expect("a dirent").name())
}

/// What @d, a dirent in directory @dir, points at - an inode in @dir's
/// subvolume, or a subvolume's root: as bch2_dirent_read_target(). None for a
/// subvolume's dirent that lists in another subvolume.
pub fn read_target(
    trans: &BtreeTrans<'_>,
    dir:   c::subvol_inum,
    d:     Dirent<'_>,
) -> Result<Option<c::subvol_inum>, BchError> {
    match d.target() {
        DirentTarget::Subvol { parent, .. } if u64::from(parent) != dir.subvol => Ok(None),
        DirentTarget::Subvol { child, .. } => {
            let s = subvolume::get(trans, child, true)?;
            Ok(Some(c::subvol_inum { subvol: child as u64, inum: u64::from_le(s.inode) }))
        }
        DirentTarget::Inode(inum) => Ok(Some(c::subvol_inum { subvol: dir.subvol, inum })),
    }
}

/// For C's VFS lookups: bch2_dirent_read_target(). 1 for a dirent @dir
/// doesn't list.
///
/// # Safety
/// The arguments are the C function's, valid for the call; @d is from the
/// btree.
#[no_mangle]
pub unsafe extern "C" fn bch2_dirent_read_target(
    trans:  *mut c::btree_trans,
    dir:    c::subvol_inum,
    d:      c::bkey_s_c_dirent,
    target: *mut c::subvol_inum,
) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };
    let k = c::bkey_s_c::from(d);

    match read_target(&trans, dir, Dirent::new(BkeySC::from(&k)).expect("a dirent")) {
        Ok(Some(t)) => {
            unsafe { *target = t };
            0
        }
        Ok(None) => 1,
        Err(e)   => -e.raw(),
    }
}

/// What a lookup found: its target - ENOENT for a dirent @dir doesn't list.
fn lookup_target(
    trans: &BtreeTrans<'_>,
    dir:   c::subvol_inum,
    k:     BkeySC<'_>,
) -> Result<c::subvol_inum, BchError> {
    read_target(trans, dir, Dirent::new(k).expect("a dirent"))?
        .ok_or(BchError::from(c::ENOENT))
}

/// What @name in directory @dir points at, as seen in @snapshot: as
/// bch2_dirent_lookup_snapshot(). @iter is left at the dirent.
#[allow(clippy::too_many_arguments)]
pub fn lookup_snapshot<'t>(
    t:         &TransAttempt<'_, 't>,
    iter:      &mut BtreeIter<'t>,
    dir:       c::subvol_inum,
    snapshot:  u32,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
    flags:     BtreeIterFlags,
) -> Result<c::subvol_inum, BchError> {
    let lookup_name = maybe_casefold(t, hash_info, name)?;
    let k = str_hash::lookup_in_snapshot::<Dirents>(t, iter, hash_info, dir, lookup_name,
                                                    flags, snapshot)?;
    lookup_target(t, dir, k)
}

/// For C's unlink: bch2_dirent_lookup_snapshot().
///
/// # Safety
/// The arguments are the C function's, valid for the call; @trans has an
/// attempt in progress, and @iter is the caller's.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn bch2_dirent_lookup_snapshot(
    trans:     *mut c::btree_trans,
    iter:      *mut c::btree_iter,
    dir:       c::subvol_inum,
    snapshot:  u32,
    hash_info: *const c::bch_hash_info,
    name:      *const c::qstr,
    inum:      *mut c::subvol_inum,
    flags:     u32,
) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };

    let ret = lookup_snapshot(&trans.attempt_in_progress(), unsafe { BtreeIter::borrow_raw(iter) },
                              dir, snapshot, unsafe { &*hash_info },
                              unsafe { qstr_name(&*name) }, BtreeIterFlags::from_bits_retain(flags));
    ret_to_c(ret.map(|i| unsafe { *inum = i }))
}

/// The dirent @name in directory @dir, through @iter - looked up by its
/// casefolded name, in a casefolded directory.
pub fn lookup_key<'i, 't>(
    t:         &'i TransAttempt<'_, 't>,
    iter:      &'i mut BtreeIter<'t>,
    dir:       c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
    flags:     BtreeIterFlags,
) -> Result<BkeySC<'i>, BchError> {
    let lookup_name = maybe_casefold(t, hash_info, name)?;
    str_hash::lookup::<Dirents>(t, iter, hash_info, dir, lookup_name, flags)
}

/// For C's VFS lookup, which goes on to check the dirent against its inode:
/// lookup_key(), the key or an error.
///
/// # Safety
/// The arguments are the C function's, valid for the call; @trans has an
/// attempt in progress, and @iter is the caller's.
#[no_mangle]
pub unsafe extern "C" fn bch2_dirent_lookup_key(
    trans:     *mut c::btree_trans,
    iter:      *mut c::btree_iter,
    dir:       c::subvol_inum,
    hash_info: *const c::bch_hash_info,
    name:      *const c::qstr,
) -> c::bkey_s_c {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };

    match lookup_key(&trans.attempt_in_progress(), unsafe { BtreeIter::borrow_raw(iter) }, dir,
                     unsafe { &*hash_info }, unsafe { qstr_name(&*name) },
                     BtreeIterFlags::empty()) {
        Ok(k)  => k.to_raw(),
        Err(e) => c::bkey_s_c {
            k: (-(e.raw() as isize)) as *const c::bkey,
            v: core::ptr::null(),
        },
    }
}

/// For C's unlink: delete the dirent at @iter, leaving a whiteout if a later
/// one in the same probe sequence needs one - str_hash::delete_at().
///
/// # Safety
/// The arguments are the C function's, valid for the call; @trans has an
/// attempt in progress, and @iter is the caller's.
#[no_mangle]
pub unsafe extern "C" fn bch2_dirent_delete_at(
    trans:     *mut c::btree_trans,
    hash_info: *const c::bch_hash_info,
    iter:      *mut c::btree_iter,
    flags:     c::btree_iter_update_trigger_flags,
) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };

    ret_to_c(str_hash::delete_at::<Dirents>(&trans.attempt_in_progress(), unsafe { &*hash_info },
                                            unsafe { BtreeIter::borrow_raw(iter) },
                                            UpdateTriggerFlags::from_bits_retain(flags.0)))
}

/// What @name in directory @dir points at: as bch2_dirent_lookup_trans().
/// @iter is left at the dirent.
pub fn lookup_trans<'t>(
    t:         &TransAttempt<'_, 't>,
    iter:      &mut BtreeIter<'t>,
    dir:       c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
    flags:     BtreeIterFlags,
) -> Result<c::subvol_inum, BchError> {
    lookup_target(t, dir, lookup_key(t, iter, dir, hash_info, name, flags)?)
}

/// What @name in directory @dir points at: as bch2_dirent_lookup().
pub fn lookup(
    fs:        &Fs,
    dir:       c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
) -> Result<c::subvol_inum, BchError> {
    let trans = crate::btree_trans!(fs);

    lockrestart_do(&trans, |t| {
        let mut iter = BtreeIter::uninit();
        lookup_trans(t, &mut iter, dir, hash_info, name, BtreeIterFlags::empty())
    })
}

/// For C's VFS and ioctls: bch2_dirent_lookup().
///
/// # Safety
/// The arguments are the C function's, valid for the call.
#[no_mangle]
pub unsafe extern "C" fn bch2_dirent_lookup(
    c:         *mut c::bch_fs,
    dir:       c::subvol_inum,
    hash_info: *const c::bch_hash_info,
    name:      *const c::qstr,
    inum:      *mut c::subvol_inum,
) -> c_int {
    let fs = unsafe { Fs::borrow_raw(c) };
    ret_to_c(lookup(&fs, dir, unsafe { &*hash_info }, unsafe { qstr_name(&*name) })
             .map(|i| unsafe { *inum = i }))
}

/// Whether directory @dir is empty as seen in @snapshot - subvolume
/// dirents count only in their parent, @subvol: as bch2_empty_dir_snapshot().
/// ENOTEMPTY_dir_not_empty if it isn't.
pub fn empty_dir_snapshot(
    t:        &TransAttempt<'_, '_>,
    dir:      u64,
    subvol:   u32,
    snapshot: u32,
) -> Result<(), BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::dirents, spos(dir, 0, snapshot),
                                  BtreeIterFlags::empty());
    iter.for_each_max_norestart(t, pos(dir, u64::MAX), |_, k| {
        let Some(d) = Dirent::new(k) else { return Ok(()) };
        match d.target() {
            DirentTarget::Subvol { parent, .. } if parent != subvol => Ok(()),
            _ => Err(t.fs().err(bch_errcode::BCH_ERR_ENOTEMPTY_dir_not_empty)),
        }
    })
}

/// For C's inode deletion: bch2_empty_dir_snapshot().
///
/// # Safety
/// @trans is valid for the call.
#[no_mangle]
pub unsafe extern "C" fn bch2_empty_dir_snapshot(
    trans:    *mut c::btree_trans,
    dir:      u64,
    subvol:   u32,
    snapshot: u32,
) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };
    ret_to_c(empty_dir_snapshot(&trans.attempt_in_progress(), dir, subvol, snapshot))
}

/// Whether directory @dir is empty, as its subvolume sees it: as
/// bch2_empty_dir_trans(). ENOTEMPTY_dir_not_empty if it isn't.
pub fn empty_dir_trans(t: &TransAttempt<'_, '_>, dir: c::subvol_inum) -> Result<(), BchError> {
    let snapshot = subvolume::get_snapshot(t, dir.subvol as u32)?;
    empty_dir_snapshot(t, dir.inum, dir.subvol as u32, snapshot)
}

/// For C's rmdir, rename and xattrs: bch2_empty_dir_trans().
///
/// # Safety
/// @trans is valid for the call.
#[no_mangle]
pub unsafe extern "C" fn bch2_empty_dir_trans(trans: *mut c::btree_trans, dir: c::subvol_inum) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };
    ret_to_c(empty_dir_trans(&trans.attempt_in_progress(), dir))
}

/// The VFS's dir_context, readdir's output.
struct DirContext<'c>(&'c mut c::dir_context);

impl DirContext<'_> {
    fn pos(&self) -> u64 {
        self.0.pos as u64
    }

    /// Fault in getdents64's buffer up front, so emit()'s fast path, which
    /// writes holding locks, doesn't fault: as bch2_readdir_fault_in().
    fn fault_in(&mut self) {
        unsafe { c::bch2_readdir_fault_in(self.0) }
    }

    /// Emit @d, pointing at @target - which may unlock @trans: as
    /// bch2_dir_emit(). False once the buffer is full.
    fn emit(&mut self, trans: &BtreeTrans<'_>, d: Dirent<'_>, target: c::subvol_inum)
        -> Result<bool, BchError>
    {
        let d = d.k().to_c_dirent().expect("a dirent");
        match unsafe { c::bch2_dir_emit(trans.raw(), self.0, d, target) } {
            0              => Ok(true),
            ret if ret > 0 => Ok(false),
            ret            => Err(BchError::from_raw(-ret)),
        }
    }
}

/// Emit directory @dir's entries to @ctx, from ctx->pos: as bch2_readdir().
///
/// Each dirent is checked against its hash first - one in the wrong place is
/// repaired, as fsck would, and skipped. Emitting is C's, bch2_dir_emit():
/// the getdents64 fast path writes to userspace holding locks, and anything
/// else unlocks - the walk relocks at the next key.
pub fn readdir(
    fs:        &Fs,
    dir:       c::subvol_inum,
    hash_info: &mut c::bch_hash_info,
    ctx:       &mut c::dir_context,
) -> Result<(), BchError> {
    let mut ctx = DirContext(ctx);
    ctx.fault_in();

    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::dirents, pos(dir.inum, ctx.pos()),
                                  BtreeIterFlags::empty());

    subvolume::for_each_in_subvolume_max_in_trans(&trans, &mut iter, pos(dir.inum, u64::MAX),
                                                  dir.subvol as u32, BtreeIterFlags::empty(), |k| {
        let Some(d) = Dirent::new(k) else { return Ok(ControlFlow::Continue(())) };

        let mut need_second_pass = false;
        match str_hash::check_key::<Dirents>(&trans, None, hash_info, k, &mut need_second_pass) {
            Err(e) if e.matches(bch_errcode::BCH_ERR_str_hash_key_repaired) =>
                return Ok(ControlFlow::Continue(())),
            r => r?,
        }

        let Some(target) = read_target(&trans, dir, d)? else {
            return Ok(ControlFlow::Continue(()));
        };

        Ok(if ctx.emit(&trans, d, target)? {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        })
    })
}

/// For C's VFS and FUSE: bch2_readdir().
///
/// # Safety
/// The arguments are the C function's, valid for the call.
#[no_mangle]
pub unsafe extern "C" fn bch2_readdir(
    c:         *mut c::bch_fs,
    dir:       c::subvol_inum,
    hash_info: *mut c::bch_hash_info,
    ctx:       *mut c::dir_context,
) -> c_int {
    let fs = unsafe { Fs::borrow_raw(c) };
    ret_to_c(readdir(&fs, dir, unsafe { &mut *hash_info }, unsafe { &mut *ctx }))
}

// ── Rename ───────────────────────────────────────────────────────────────

/// What rename keeps of a dirent it replaces - read before anything else
/// moves the iterator that found it.
#[derive(Clone, Copy)]
struct OldDirent {
    d_type:   u8,
    target:   c::bch_dirent__bindgen_ty_1,
    snapshot: u32,
}

impl OldDirent {
    fn of(d: Dirent<'_>) -> Self {
        OldDirent { d_type: d.d_type(), target: d.v().__bindgen_anon_1, snapshot: d.k().k.p.snapshot }
    }

    fn is_subvol(&self) -> bool {
        self.d_type as u32 == c::DT_SUBVOL
    }

    /// Point @dst at what this pointed at: as dirent_copy_target().
    fn copy_target_to(&self, dst: &mut TransBkey<'_, '_>) {
        let d = dst.k_i_mut().as_mut_dirent().expect("a dirent");
        d.__bindgen_anon_1 = self.target;
        d.set_d_type(self.d_type);
    }
}

/// If @k is a subvolume's dirent, it lists in @dir's subvolume - the
/// directory it's being moved into.
fn set_subvol_parent(k: &mut TransBkey<'_, '_>, dir: c::subvol_inum) {
    if let Some(d) = k.k_i_mut().as_mut_dirent() {
        if d.d_type() as u32 == c::DT_SUBVOL {
            d.set_parent_subvol(dir.subvol as u32);
        }
    }
}

/// What a rename did: what the names pointed at before, and where the dirents
/// now named src and dst went.
#[derive(Default)]
pub struct Renamed {
    pub src_inum:   c::subvol_inum,
    pub dst_inum:   c::subvol_inum,
    pub src_offset: u64,
    pub dst_offset: u64,
}

/// Rename @src_name in @src_dir to @dst_name in @dst_dir - for
/// BCH_RENAME_OVERWRITE replacing what's there, for BCH_RENAME_EXCHANGE
/// swapping the two: as bch2_dirent_rename().
///
/// Whether dst exists isn't checked for a plain rename: the VFS has.
#[allow(clippy::too_many_arguments)]
pub fn rename(
    t:        &TransAttempt<'_, '_>,
    src_dir:  c::subvol_inum,
    src_hash: &c::bch_hash_info,
    dst_dir:  c::subvol_inum,
    dst_hash: &c::bch_hash_info,
    src_name: &OsStr,
    dst_name: &OsStr,
    mode:     c::bch_rename_mode,
) -> Result<Renamed, BchError> {
    use c::bch_rename_mode::*;

    let mut r = Renamed::default();
    let mut src_iter = BtreeIter::uninit();
    let mut dst_iter = BtreeIter::uninit();

    let src_lookup = maybe_casefold(t, src_hash, src_name)?;
    let old_src = {
        let k = str_hash::lookup::<Dirents>(t, &mut src_iter, src_hash, src_dir,
                                            src_lookup, BtreeIterFlags::INTENT)?;
        r.src_inum = lookup_target(t, src_dir, k)?;
        OldDirent::of(Dirent::new(k).expect("a dirent"))
    };

    let dst_lookup = maybe_casefold(t, dst_hash, dst_name)?;
    let dst_hashed = pos(dst_dir.inum, hash(dst_hash, dst_lookup));

    let old_dst = if mode == BCH_RENAME {
        str_hash::hole::<Dirents>(t, &mut dst_iter, dst_hash, dst_dir, dst_lookup)?;
        None
    } else {
        let k = str_hash::lookup::<Dirents>(t, &mut dst_iter, dst_hash, dst_dir,
                                            dst_lookup, BtreeIterFlags::INTENT)?;
        r.dst_inum = lookup_target(t, dst_dir, k)?;
        Some(OldDirent::of(Dirent::new(k).expect("a dirent")))
    };

    let src_pos = src_iter.pos();
    let dst_pos = dst_iter.pos();

    if mode != BCH_RENAME_EXCHANGE {
        r.src_offset = dst_pos.offset;
    }

    let cf = |hash: &c::bch_hash_info, lookup| (!hash.cf_encoding.is_null()).then_some(lookup);

    let mut new_dst = create_key(t, dst_hash, dst_dir, 0, dst_name, cf(dst_hash, dst_lookup), 0)?;
    old_src.copy_target_to(&mut new_dst);
    new_dst.k_mut().p = dst_pos;

    // Positions as the hash table orders them, by inode and offset:
    let le = |l: c::bpos, r: c::bpos| (l.inode, l.offset) <= (r.inode, r.offset);
    let lt = |l: c::bpos, r: c::bpos| (l.inode, l.offset) <  (r.inode, r.offset);

    let mut new_src = if mode == BCH_RENAME_EXCHANGE {
        let mut k = create_key(t, src_hash, src_dir, 0, src_name, cf(src_hash, src_lookup), 0)?;
        old_dst.expect("exchange looked dst up").copy_target_to(&mut k);
        k.k_mut().p = src_pos;
        Some(k)
    } else {
        let mut k = t.bkey_alloc_init(0, c::bch_bkey_type::KEY_TYPE_deleted.0 as u8, src_pos)?;

        if le(dst_hashed, src_pos) && lt(src_pos, dst_pos) {
            // A hash collision for the new dst, and src - the key we're
            // deleting - is between dst's hashed slot and the slot it's going
            // in: deleting src would break the probe sequence.
            if mode == BCH_RENAME {
                // Not overwriting: new dst can go in src's slot instead.
                None
            } else {
                // Overwriting: new dst has to go where old dst is, so src
                // gets a whiteout.
                k.k_mut().type_ = c::bch_bkey_type::KEY_TYPE_hash_whiteout.0 as u8;
                Some(k)
            }
        } else {
            if str_hash::needs_whiteout::<Dirents>(t, src_hash, &src_iter)? {
                k.k_mut().type_ = c::bch_bkey_type::KEY_TYPE_hash_whiteout.0 as u8;
            }
            Some(k)
        }
    };

    let (new_src, new_dst_pos) = match new_src.take() {
        None => {
            new_dst.k_mut().p = src_pos;
            (new_dst, src_pos)
        }
        Some(mut new_src) => {
            set_subvol_parent(&mut new_dst, dst_dir);
            if mode == BCH_RENAME_EXCHANGE {
                set_subvol_parent(&mut new_src, src_dir);
            }
            t.update(&dst_iter, &new_dst, UpdateTriggerFlags::empty())?;
            (new_src, dst_pos)
        }
    };

    // A subvolume's dirent is deleted outright, not whited out in this
    // snapshot: there's only ever the one dirent for a subvolume, visible in
    // its parent - versions of it in other snapshots would only confuse fsck.
    let new_src_pos = new_src.k().p;
    let new_src_deleted = new_src.k().type_ == c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
    let delete_src = old_src.is_subvol() && new_src_pos.snapshot != old_src.snapshot;
    let delete_dst = old_dst.is_some_and(|o| o.is_subvol() && new_dst_pos.snapshot != o.snapshot);

    if !delete_src || !new_src_deleted {
        t.update(&src_iter, &new_src, UpdateTriggerFlags::empty())?;
    }

    if delete_src {
        src_iter.set_snapshot(old_src.snapshot);
        src_iter.traverse(t)?;
        t.delete_at(&src_iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    }

    if let Some(o) = old_dst.filter(|_| delete_dst) {
        dst_iter.set_snapshot(o.snapshot);
        dst_iter.traverse(t)?;
        t.delete_at(&dst_iter, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    }

    if mode == BCH_RENAME_EXCHANGE {
        r.src_offset = new_src_pos.offset;
    }
    r.dst_offset = new_dst_pos.offset;

    Ok(r)
}

/// For C's rename: bch2_dirent_rename().
///
/// # Safety
/// The arguments are the C function's, valid for the call; @trans has an
/// attempt in progress.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn bch2_dirent_rename(
    trans:      *mut c::btree_trans,
    src_dir:    c::subvol_inum,
    src_hash:   *mut c::bch_hash_info,
    dst_dir:    c::subvol_inum,
    dst_hash:   *mut c::bch_hash_info,
    src_name:   *const c::qstr,
    src_inum:   *mut c::subvol_inum,
    src_offset: *mut u64,
    dst_name:   *const c::qstr,
    dst_inum:   *mut c::subvol_inum,
    dst_offset: *mut u64,
    mode:       c::bch_rename_mode,
) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };

    unsafe {
        *src_inum = Default::default();
        *dst_inum = Default::default();
    }

    let ret = rename(&trans.attempt_in_progress(), src_dir, unsafe { &*src_hash },
                     dst_dir, unsafe { &*dst_hash },
                     unsafe { qstr_name(&*src_name) }, unsafe { qstr_name(&*dst_name) }, mode);
    ret_to_c(ret.map(|r| unsafe {
        *src_inum   = r.src_inum;
        *dst_inum   = r.dst_inum;
        *src_offset = r.src_offset;
        *dst_offset = r.dst_offset;
    }))
}

// ── fsck ─────────────────────────────────────────────────────────────────

/// The first version of inode @inum, in any snapshot.
fn lookup_first_inode(t: &TransAttempt<'_, '_>, inum: u64)
    -> Result<c::bch_inode_unpacked, BchError>
{
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::inodes, pos(0, inum),
                                  BtreeIterFlags::ALL_SNAPSHOTS);

    let ret = iter.for_each_norestart(t, |_, k| Ok(
        if k.k.p.offset != inum {
            ControlFlow::Break(None)
        } else if inode::bkey_is_inode(k.k) {
            ControlFlow::Break(Some(inode::unpack(fs, k)))
        } else {
            ControlFlow::Continue(())
        }))
        .and_then(|found| found.ok_or_else(|| fs.err(bch_errcode::BCH_ERR_ENOENT_inode)));

    crate::bch_err_msg!(fs, ret, "fetching inode {inum}")
}

/// Delete the dirent at @pos, as fsck repair: as bch2_fsck_remove_dirent().
pub fn fsck_remove(t: &TransAttempt<'_, '_>, pos: c::bpos) -> Result<(), BchError> {
    let dir = lookup_first_inode(t, pos.inode)?;
    let hash_info = str_hash::hash_info_init_unchecked(t.fs(), &dir);

    let mut iter = BtreeIter::new(t, c::btree_id::dirents, pos, BtreeIterFlags::INTENT);
    iter.traverse(t)?;
    str_hash::delete_at::<Dirents>(t, &hash_info, &mut iter,
                                   UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/// For C's namei fsck: bch2_fsck_remove_dirent().
///
/// # Safety
/// @trans is valid for the call.
#[no_mangle]
pub unsafe extern "C" fn bch2_fsck_remove_dirent(trans: *mut c::btree_trans, pos: c::bpos) -> c_int {
    let trans = unsafe { BtreeTrans::borrow_raw(trans) };
    ret_to_c(fsck_remove(&trans.attempt_in_progress(), pos))
}
