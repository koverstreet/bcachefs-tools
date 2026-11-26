// SPDX-License-Identifier: GPL-2.0

//! Dirents (fs/dirent.c): directory entries - a hash table per directory in
//! the dirents btree, keyed by the hash of the name - and the operations on
//! them: creating, looking up, iterating.
//!
//! The value itself - its target, its names, and what may be trusted of
//! them - is dirent_format.rs. A dirent's consistency with the inode it names
//! is namei's (namei.rs, as in C).

#![allow(non_camel_case_types)]

use cstruct_macros::bch_enum;

#[bch_enum(closed)]
#[repr(u32)]
pub enum RenameMode {
    BCH_RENAME,
    BCH_RENAME_OVERWRITE,
    BCH_RENAME_EXCHANGE,
}

pub use super::dirent_format::{d_type_str, set_field, DirentName, DirentTarget, RawNames};
use crate::btree::bkey::prelude::*;
use super::dirent_format::D_NAME_OFFSET;
#[cfg(CONFIG_UNICODE)]
use super::dirent_format::D_NAMES_OFFSET;
use crate::util::vstructs::cast_plain_mut;

use crate::btree::bkey::{pos, spos, BKEY_U64S, BkeyI, BkeyS, BkeySC};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{
    lockrestart_do, BtreeIter, BtreeIterFlags, TransAttempt, TransBkey,
    UpdateTriggerFlags,
};
use crate::c;
use crate::posix::{dirent_format, inode};
use crate::posix::dirent_format::{BCH_NAME_MAX, DT_SUBVOL, DirentCfNameBlock,
    DirentSubvol, bch_dirent_target};
use crate::posix::inode::InodeUnpacked;
use crate::snapshots::{subvolume::SubvolInum, subvolume};
use crate::errcode::{Errcode, BchError};
use crate::format::{Bpos, Btree, BkeyType};
use crate::fs::Fs;
use crate::posix::str_hash::{self, HashTable, HashInfo};
use crate::os::unicode::UnicodeMap;
use crate::util::os_str::{OsStr, OsStrExt};
use core::ops::ControlFlow;

// ── A valid dirent ───────────────────────────────────────────────────────

/// A valid dirent - one that has passed validate, as every key from the
/// btree has.
#[derive(Clone, Copy)]
pub struct Dirent<'k> {
    k:     BkeySC<'k, dirent_format::Dirent>,
    v:     &'k dirent_format::Dirent,
    names: DirentName<'k>,
}

impl<'k> Dirent<'k> {
    /// @k as a dirent, if it is one.
    pub fn new(k: BkeySC<'k>) -> Option<Self> {
        k.arm::<dirent_format::Dirent>().map(Dirent::from)
    }

    pub fn k(&self) -> BkeySC<'k, dirent_format::Dirent> { self.k }

    pub fn v(&self) -> &'k dirent_format::Dirent { self.v }

    pub fn d_type(&self) -> u8 { self.v.d_type() }

    pub fn target(&self) -> DirentTarget { self.v.target() }

    pub fn names(&self) -> DirentName<'k> { self.names }

    /// The name as given.
    pub fn name(&self) -> &'k OsStr { self.names.name() }

    /// The name the dirent is hashed and looked up by.
    pub fn lookup_name(&self) -> &'k OsStr { self.names.lookup_name() }
}

impl<'k> From<BkeySC<'k, dirent_format::Dirent>> for Dirent<'k> {
    fn from(k: BkeySC<'k, dirent_format::Dirent>) -> Self {
        let v = k.v();
        let names = RawNames::new(k).names()
            .expect("dirent names within the value, as validate checked");
        Dirent { k, v, names }
    }
}


// ── The hash table ───────────────────────────────────────────────────────

/// The dirents btree, as a hash table.
pub struct Dirents;

/// Keys are looked up by the name they're hashed by - casefolded, in a
/// casefolded directory.
impl HashTable for Dirents {
    type Key<'k> = OsStr;
    type Val = dirent_format::Dirent;

    const BTREE: Btree = Btree::dirents;

    fn hash_key(info: &HashInfo, key: &OsStr) -> u64 {
        hash(info, key)
    }

    fn hash_bkey(info: &HashInfo, k: BkeySC<'_, dirent_format::Dirent>) -> u64 {
        hash(info, Dirent::from(k).lookup_name())
    }

    fn matches(k: BkeySC<'_, dirent_format::Dirent>, key: &OsStr) -> bool {
        Dirent::from(k).lookup_name() == key
    }

    fn same_name(a: BkeySC<'_, dirent_format::Dirent>, b: BkeySC<'_, dirent_format::Dirent>) -> bool {
        Dirent::from(a).lookup_name() == Dirent::from(b).lookup_name()
    }

    /// A subvolume's dirent lists only in its parent subvolume.
    fn is_visible(inum: SubvolInum, k: BkeySC<'_, dirent_format::Dirent>) -> bool {
        match Dirent::from(k).target() {
            DirentTarget::Subvol { parent, .. } => u64::from(parent) == inum.subvol,
            DirentTarget::Inode(_)              => true,
        }
    }
}

/// @name's hash under @info, its offset in its directory: as
/// bch2_dirent_hash(). Offsets 0 and 1 are kept for . and .., which aren't
/// stored.
pub fn hash(info: &HashInfo, name: &OsStr) -> u64 {
    str_hash::hash_parts(info, &[name.as_bytes()], true).max(2)
}

// ── Making dirents ───────────────────────────────────────────────────────

/// A dirent key at @pos with room for any name, to be filled in with
/// copy_target() and init_name().
pub fn alloc_max<'a, 't>(t: &TransAttempt<'a, 't>, pos: Bpos)
    -> Result<TransBkey<'a, 't, dirent_format::Dirent>, BchError>
{
    t.bkey_alloc_init::<dirent_format::Dirent>(u8::MAX as usize - BKEY_U64S, pos)
}

/// Point @dst at what @src points at: as dirent_copy_target().
pub fn copy_target(dst: &mut TransBkey<'_, '_, dirent_format::Dirent>, src: Dirent<'_>) {
    let d = dst.v_mut();
    d.target = src.v().target;
    d.set_d_type(src.d_type());
}

/// Give @k, sized to the room it has, the name @name, and size it to fit: as
/// bch2_dirent_init_name(). In a casefolded directory - per @hash_info - the
/// casefolded name follows it: @cf_name if the caller has it, otherwise made
/// here.
pub fn init_name(
    fs:        &Fs,
    k:         &mut BkeyI<dirent_format::Dirent>,
    hash_info: &HashInfo,
    name:      &OsStr,
    cf_name:   Option<&OsStr>,
) -> Result<(), BchError> {
    let name = name.as_bytes();
    let cf_name = cf_name.map(OsStrExt::as_bytes);

    if name.len() > BCH_NAME_MAX as usize {
        return Err(Errcode::ENAMETOOLONG_dirent_name.into());
    }

    let casefold = hash_info.cf_encoding.is_some();
    debug_assert!(casefold || cf_name.is_none(), "a casefolded name for a dirent that isn't");
    k.v_mut().set_d_casefold(casefold as u8);

    let end = match hash_info.cf_encoding {
        None => {
            let v = BkeyS::<dirent_format::Dirent>::from(&mut *k).into_val_bytes_mut();
            v[D_NAME_OFFSET..][..name.len()].copy_from_slice(name);
            D_NAME_OFFSET + name.len()
        }
        Some(map) => {
            fs.casefold_enabled()?;
            casefold_name(k, map, name, cf_name)?
        }
    };

    // NUL padding to the end of the u64: what the name's length is read from.
    let val_u64s = end.div_ceil(8);
    (&mut *k).val_bytes_mut()[end..val_u64s * 8].fill(0);

    assert!(BKEY_U64S + val_u64s <= k.k.u64s as usize,
            "dirent name ({} bytes) overruns its key ({} u64s)", end, k.k.u64s);
    k.k.u64s = (BKEY_U64S + val_u64s) as u8;
    Ok(())
}

/// The casefolded half of init_name(): both names into the name block, and
/// their lengths. Where the names end.
#[cfg(CONFIG_UNICODE)]
fn casefold_name(
    k:       &mut BkeyI<dirent_format::Dirent>,
    map:     &UnicodeMap,
    name:    &[u8],
    cf_name: Option<&[u8]>,
) -> Result<usize, BchError> {
    let v = BkeyS::<dirent_format::Dirent>::from(&mut *k).into_val_bytes_mut();
    let (names, out) = v[D_NAMES_OFFSET..].split_at_mut(name.len());
    names.copy_from_slice(name);

    let cf_len = match cf_name {
        Some(cf) => {
            out[..cf.len()].copy_from_slice(cf);
            cf.len()
        }
        None => map.casefold(OsStr::from_bytes(name), out)?,
    };

    let v = BkeyS::<dirent_format::Dirent>::from(&mut *k).into_val_bytes_mut();
    let cf_block = cast_plain_mut::<DirentCfNameBlock>(&mut v[D_NAME_OFFSET..])
        .expect("a dirent's name block");
    cf_block.d_name_len    = (name.len() as u16).into();
    cf_block.d_cf_name_len = (cf_len as u16).into();

    Ok(D_NAMES_OFFSET + name.len() + cf_len)
}

#[cfg(not(CONFIG_UNICODE))]
fn casefold_name(
    _k:       &mut BkeyI<dirent_format::Dirent>,
    map:      &UnicodeMap,
    _name:    &[u8],
    _cf_name: Option<&[u8]>,
) -> Result<usize, BchError> {
    match *map {}
}

/// A new dirent in directory @dir named @name, of type @d_type, pointing at
/// @target - an inode, or for DT_SUBVOL a subvolume - its casefolded name
/// @cf_name if the caller has it: as bch2_dirent_create_key(). Its position
/// is for the caller to fill in.
pub fn create_key<'a, 't>(
    t:         &TransAttempt<'a, 't>,
    hash_info: &HashInfo,
    dir:       SubvolInum,
    d_type:    u8,
    name:      &OsStr,
    cf_name:   Option<&OsStr>,
    target:    u64,
) -> Result<TransBkey<'a, 't, dirent_format::Dirent>, BchError> {
    let mut k = alloc_max(t, pos(dir.inum, 0))?;

    let d = k.v_mut();
    d.set_d_type(d_type);
    if d_type as u32 != DT_SUBVOL {
        d.target.d_inum = target.into();
    } else {
        d.target.subvol = DirentSubvol {
            d_child_subvol:  (target as u32).into(),
            d_parent_subvol: (dir.subvol as u32).into(),
        };
    }

    init_name(t.fs(), k.key_mut(), hash_info, name, cf_name)?;
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
    dir:          &InodeUnpacked,
    d_type:       u8,
    name:         &OsStr,
    target:       DirentTarget,
    dir_offset:   &mut u64,
    set_flags:    str_hash::SetFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    let dir_inum = SubvolInum { subvol: subvol as u64, inum: dir.bi_inum };
    let hash_info = str_hash::hash_info_init(t.fs(), dir)?;

    // A subvolume dirent's parent is @subvol:
    let target = match target {
        DirentTarget::Subvol { child, .. } => child as u64,
        DirentTarget::Inode(inum)          => inum,
    };

    let k = create_key(t, &hash_info, dir_inum, d_type, name, None, target)?;
    *dir_offset = str_hash::set_in_snapshot::<Dirents>(t, &hash_info, dir_inum, snapshot, k,
                                                       set_flags, update_flags)?.offset;
    Ok(())
}

/// Create dirent @name in directory @dir, pointing at @target - for
/// DT_SUBVOL, a subvolume: as bch2_dirent_create(). Where it went is returned
/// in @dir_offset.
#[allow(clippy::too_many_arguments)]
pub fn create(
    t:            &TransAttempt<'_, '_>,
    dir:          SubvolInum,
    dir_u:        &InodeUnpacked,
    d_type:       u8,
    name:         &OsStr,
    target:       u64,
    dir_offset:   &mut u64,
    set_flags:    str_hash::SetFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    let hash_info = str_hash::hash_info_init(t.fs(), dir_u)?;

    let k = create_key(t, &hash_info, dir, d_type, name, None, target)?;
    *dir_offset = str_hash::set::<Dirents>(t, &hash_info, dir, k, set_flags, update_flags)?.offset;
    Ok(())
}

// ── Casefolding ──────────────────────────────────────────────────────────

/// @name casefolded with @map, in transaction memory: as bch2_casefold().
#[cfg(CONFIG_UNICODE)]
pub fn casefold<'a>(
    t:    &TransAttempt<'a, '_>,
    map:  &UnicodeMap,
    name: &OsStr,
) -> Result<&'a OsStr, BchError> {
    t.fs().casefold_enabled()?;

    let buf = t.kmalloc(BCH_NAME_MAX as usize + 1)?;
    let len = map.casefold(name, buf)?;
    Ok(OsStr::from_bytes(&buf[..len]))
}

#[cfg(not(CONFIG_UNICODE))]
pub fn casefold<'a>(
    _t:    &TransAttempt<'a, '_>,
    map:   &UnicodeMap,
    _name: &OsStr,
) -> Result<&'a OsStr, BchError> {
    match *map {}
}

/// The name @name is hashed and looked up by in a directory with @info -
/// casefolded, if the directory is: as bch2_maybe_casefold().
pub fn maybe_casefold<'a>(
    t:    &TransAttempt<'a, '_>,
    info: &HashInfo,
    name: &'a OsStr,
) -> Result<&'a OsStr, BchError> {
    match info.cf_encoding {
        None      => Ok(name),
        Some(map) => casefold(t, map, name),
    }
}

// ── Reading dirents ──────────────────────────────────────────────────────

/// What @d, a dirent in directory @dir, points at - an inode in @dir's
/// subvolume, or a subvolume's root. None for a subvolume's dirent that lists
/// in another subvolume.
pub fn read_target(
    t:   &TransAttempt<'_, '_>,
    dir: SubvolInum,
    d:   Dirent<'_>,
) -> Result<Option<SubvolInum>, BchError> {
    match d.target() {
        DirentTarget::Subvol { parent, .. } if u64::from(parent) != dir.subvol => Ok(None),
        DirentTarget::Subvol { child, .. } => {
            let s = subvolume::get(t, child, true)?;
            Ok(Some(SubvolInum { subvol: child as u64, inum: s.inode.get() }))
        }
        DirentTarget::Inode(inum) => Ok(Some(SubvolInum { subvol: dir.subvol, inum })),
    }
}

/// What a lookup found: its target - ENOENT for a dirent @dir doesn't list.
pub fn lookup_target(
    t:   &TransAttempt<'_, '_>,
    dir: SubvolInum,
    k:   BkeySC<'_, dirent_format::Dirent>,
) -> Result<SubvolInum, BchError> {
    read_target(t, dir, Dirent::from(k))?
        .ok_or_else(|| t.fs().err(Errcode::ENOENT_dirent_target))
}

/// What @name in directory @dir points at, as seen in @snapshot: as
/// bch2_dirent_lookup_snapshot(). @iter is left at the dirent.
#[allow(clippy::too_many_arguments)]
pub fn lookup_snapshot<'a>(
    t:         &TransAttempt<'a, '_>,
    iter:      &mut BtreeIter<'a>,
    dir:       SubvolInum,
    snapshot:  u32,
    hash_info: &HashInfo,
    name:      &OsStr,
    flags:     BtreeIterFlags,
) -> Result<SubvolInum, BchError> {
    let lookup_name = maybe_casefold(t, hash_info, name)?;
    let k = str_hash::lookup_in_snapshot::<Dirents>(t, iter, hash_info, dir, lookup_name,
                                                    flags, snapshot)?;
    lookup_target(t, dir, k)
}

/// The dirent @name in directory @dir, through @iter - looked up by its
/// casefolded name, in a casefolded directory.
pub fn lookup_key<'i, 'a>(
    t:         &'i TransAttempt<'a, '_>,
    iter:      &'i mut BtreeIter<'a>,
    dir:       SubvolInum,
    hash_info: &HashInfo,
    name:      &OsStr,
    flags:     BtreeIterFlags,
) -> Result<BkeySC<'i, dirent_format::Dirent>, BchError> {
    let lookup_name = maybe_casefold(t, hash_info, name)?;
    str_hash::lookup::<Dirents>(t, iter, hash_info, dir, lookup_name, flags)
}

/// What @name in directory @dir points at: as bch2_dirent_lookup_trans().
/// @iter is left at the dirent.
pub fn lookup_trans<'a>(
    t:         &TransAttempt<'a, '_>,
    iter:      &mut BtreeIter<'a>,
    dir:       SubvolInum,
    hash_info: &HashInfo,
    name:      &OsStr,
    flags:     BtreeIterFlags,
) -> Result<SubvolInum, BchError> {
    lookup_target(t, dir, lookup_key(t, iter, dir, hash_info, name, flags)?)
}

/// What @name in directory @dir points at.
pub fn lookup(
    fs:        &Fs,
    dir:       SubvolInum,
    hash_info: &HashInfo,
    name:      &OsStr,
) -> Result<SubvolInum, BchError> {
    let trans = crate::btree_trans!(fs);

    lockrestart_do(&trans, |t| {
        let mut iter = BtreeIter::uninit(t.trans());
        lookup_trans(t, &mut iter, dir, hash_info, name, BtreeIterFlags::empty())
    })
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
    let mut iter = BtreeIter::new(t, Btree::dirents, spos(dir, 0, snapshot),
                                  BtreeIterFlags::empty());
    iter.for_each_max_norestart(t, pos(dir, u64::MAX), |_, k| {
        let Some(d) = Dirent::new(k) else { return Ok(()) };
        match d.target() {
            DirentTarget::Subvol { parent, .. } if parent != subvol => Ok(()),
            _ => Err(t.fs().err(Errcode::ENOTEMPTY_dir_not_empty)),
        }
    })
}

/// Whether directory @dir is empty, as its subvolume sees it: as
/// bch2_empty_dir_trans(). ENOTEMPTY_dir_not_empty if it isn't.
pub fn empty_dir_trans(t: &TransAttempt<'_, '_>, dir: SubvolInum) -> Result<(), BchError> {
    let snapshot = subvolume::get_snapshot(t, dir.subvol as u32)?;
    empty_dir_snapshot(t, dir.inum, dir.subvol as u32, snapshot)
}

/// Where readdir's entries go, and where it resumes from: a dir_context -
/// getdents' - or a Rust caller's own.
pub trait DirEmit {
    fn pos(&self) -> u64;
    fn set_pos(&mut self, pos: u64);
    /// @name, inode @ino of type @d_type, at @pos: false once full.
    fn emit(&mut self, name: &[u8], pos: u64, ino: u64, d_type: u32) -> bool;
}

impl<E: DirEmit> DirEmit for &mut E {
    fn pos(&self) -> u64 {
        (**self).pos()
    }

    fn set_pos(&mut self, pos: u64) {
        (**self).set_pos(pos)
    }

    fn emit(&mut self, name: &[u8], pos: u64, ino: u64, d_type: u32) -> bool {
        (**self).emit(name, pos, ino, d_type)
    }
}

/// Emit @d, pointing at @target, unlocking @t. False once the buffer is
/// full.
fn dir_emit(ctx: &mut impl DirEmit, t: &TransAttempt<'_, '_>, d: Dirent<'_>, target: SubvolInum) -> bool {
    let offset = d.k().k.p.offset;

    // Although not required by the kernel code, updating ctx->pos is
    // needed for the bcachefs FUSE driver. Without this update, the FUSE
    // implementation will be stuck in an infinite loop when reading
    // directories (via the bcachefs_fuse_readdir callback): its actor
    // computes each entry's resume-after cookie as its pos argument + 1.
    // In kernel space, ctx->pos is updated by the VFS code.
    ctx.set_pos(offset);

    let k = BkeyBuf::from_key(d.k().into());
    let d = Dirent::new(BkeySC::from(&k)).expect("a dirent");
    let name = d.name().as_bytes();
    let d_type = match d.d_type() as u32 {
        DT_SUBVOL => c::DT_DIR,
        d_type       => d_type,
    };

    // The actor copies to userspace and can fault, so it can't run with
    // btree locks held — drop them, then relock for the next iteration.
    t.unlock();

    let emitted = ctx.emit(name, offset, target.inum, d_type);

    if emitted {
        ctx.set_pos(offset + 1);
    }
    // Don't relock here: a restart return would cause the walk to retry
    // the current key without advancing the iter, which re-emits the
    // just-emitted dirent. Let the next peek relock transparently.
    emitted
}

/// Emit directory @dir's entries to @ctx, from ctx->pos: as bch2_readdir().
///
/// Each dirent is checked against its hash first - one in the wrong place is
/// repaired, as fsck would, and skipped. Emitting unlocks - the walk relocks
/// at the next key.
pub fn readdir(
    fs:        &Fs,
    dir:       SubvolInum,
    hash_info: &mut HashInfo,
    mut ctx:   impl DirEmit,
) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, Btree::dirents, pos(dir.inum, ctx.pos()),
                                  BtreeIterFlags::empty());

    subvolume::for_each_in_subvolume_max_in_trans(&trans, &mut iter, pos(dir.inum, u64::MAX),
                                                  dir.subvol as u32, BtreeIterFlags::empty(), |t, _, k| {
        let Some(d) = Dirent::new(k) else { return Ok(ControlFlow::Continue(())) };

        let mut need_second_pass = false;
        match str_hash::check_key::<Dirents>(t, None, hash_info, k, &mut need_second_pass) {
            Err(e) if e.matches(Errcode::str_hash_key_repaired) =>
                return Ok(ControlFlow::Continue(())),
            r => r?,
        }

        let Some(target) = read_target(t, dir, d)? else {
            return Ok(ControlFlow::Continue(()));
        };

        Ok(if dir_emit(&mut ctx, t, d, target) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        })
    })
}

// ── Rename ───────────────────────────────────────────────────────────────

/// What rename keeps of a dirent it replaces - read before anything else
/// moves the iterator that found it.
#[derive(Clone, Copy)]
struct OldDirent {
    d_type:   u8,
    target:   bch_dirent_target,
    snapshot: u32,
}

impl OldDirent {
    fn of(d: Dirent<'_>) -> Self {
        OldDirent { d_type: d.d_type(), target: d.v().target, snapshot: d.k().k.p.snapshot }
    }

    fn is_subvol(&self) -> bool {
        self.d_type as u32 == DT_SUBVOL
    }

    /// Point @dst at what this pointed at: as dirent_copy_target().
    fn copy_target_to(&self, dst: &mut TransBkey<'_, '_, dirent_format::Dirent>) {
        let d = dst.v_mut();
        d.target = self.target;
        d.set_d_type(self.d_type);
    }
}

/// If @k is a subvolume's dirent, it lists in @dir's subvolume - the
/// directory it's being moved into.
fn set_subvol_parent(k: &mut TransBkey<'_, '_>, dir: SubvolInum) {
    if let Some(d) = k.as_mut().arm::<dirent_format::Dirent>().map(|k| k.v_mut()) {
        if d.d_type() as u32 == DT_SUBVOL {
            d.set_parent_subvol(dir.subvol as u32);
        }
    }
}

/// What a rename did: what the names pointed at before, and where the dirents
/// now named src and dst went.
#[derive(Default)]
pub struct Renamed {
    pub src_inum:   SubvolInum,
    pub dst_inum:   SubvolInum,
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
    src_dir:  SubvolInum,
    src_hash: &HashInfo,
    dst_dir:  SubvolInum,
    dst_hash: &HashInfo,
    src_name: &OsStr,
    dst_name: &OsStr,
    mode:     RenameMode,
) -> Result<Renamed, BchError> {
    use self::RenameMode::*;

    let mut r = Renamed::default();
    let mut src_iter = BtreeIter::uninit(t.trans());
    let mut dst_iter = BtreeIter::uninit(t.trans());

    let src_lookup = maybe_casefold(t, src_hash, src_name)?;
    let old_src = {
        let k = str_hash::lookup::<Dirents>(t, &mut src_iter, src_hash, src_dir,
                                            src_lookup, BtreeIterFlags::INTENT)?;
        r.src_inum = lookup_target(t, src_dir, k)?;
        OldDirent::of(Dirent::from(k))
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
        Some(OldDirent::of(Dirent::from(k)))
    };

    let src_pos = src_iter.pos();
    let dst_pos = dst_iter.pos();

    if mode != BCH_RENAME_EXCHANGE {
        r.src_offset = dst_pos.offset;
    }

    let cf = |hash: &HashInfo, lookup| hash.cf_encoding.is_some().then_some(lookup);

    // Untyped, as new_src may be a deletion or a whiteout:
    let mut new_dst: TransBkey<'_, '_> = {
        let mut k = create_key(t, dst_hash, dst_dir, 0, dst_name, cf(dst_hash, dst_lookup), 0)?;
        old_src.copy_target_to(&mut k);
        k.k_mut().p = dst_pos;
        k.into()
    };

    // Positions as the hash table orders them, by inode and offset:
    let le = |l: Bpos, r: Bpos| (l.inode, l.offset) <= (r.inode, r.offset);
    let lt = |l: Bpos, r: Bpos| (l.inode, l.offset) <  (r.inode, r.offset);

    let mut new_src = if mode == BCH_RENAME_EXCHANGE {
        let mut k = create_key(t, src_hash, src_dir, 0, src_name, cf(src_hash, src_lookup), 0)?;
        old_dst.expect("exchange looked dst up").copy_target_to(&mut k);
        k.k_mut().p = src_pos;
        Some(k.into())
    } else if le(dst_hashed, src_pos) && lt(src_pos, dst_pos) {
        // A hash collision for the new dst, and src - the key we're deleting -
        // is between dst's hashed slot and the slot it's going in: deleting
        // src would break the probe sequence.
        if mode == BCH_RENAME {
            // Not overwriting: new dst can go in src's slot instead.
            None
        } else {
            // Overwriting: new dst has to go where old dst is, so src gets a
            // whiteout.
            Some(str_hash::deletion(t, src_pos, true)?)
        }
    } else {
        let whiteout = str_hash::needs_whiteout::<Dirents>(t, src_hash, &src_iter)?;
        Some(str_hash::deletion(t, src_pos, whiteout)?)
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
            t.update(&dst_iter, new_dst, UpdateTriggerFlags::empty())?;
            (new_src, dst_pos)
        }
    };

    // A subvolume's dirent is deleted outright, not whited out in this
    // snapshot: there's only ever the one dirent for a subvolume, visible in
    // its parent - versions of it in other snapshots would only confuse fsck.
    let new_src_pos = new_src.k().p;
    let new_src_deleted = new_src.k().key_type() == BkeyType::deleted;
    let delete_src = old_src.is_subvol() && new_src_pos.snapshot != old_src.snapshot;
    let delete_dst = old_dst.is_some_and(|o| o.is_subvol() && new_dst_pos.snapshot != o.snapshot);

    if !delete_src || !new_src_deleted {
        t.update(&src_iter, new_src, UpdateTriggerFlags::empty())?;
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

// ── fsck ─────────────────────────────────────────────────────────────────

/// The first version of inode @inum, in any snapshot.
fn lookup_first_inode(t: &TransAttempt<'_, '_>, inum: u64)
    -> Result<InodeUnpacked, BchError>
{
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, Btree::inodes, pos(0, inum),
                                  BtreeIterFlags::ALL_SNAPSHOTS);

    let ret = iter.for_each_norestart(t, |_, k| Ok(
        if k.k.p.offset != inum {
            ControlFlow::Break(None)
        } else if let Some(k) = k.arm::<inode::Inodes>() {
            ControlFlow::Break(Some(inode::unpack(fs, k)))
        } else {
            ControlFlow::Continue(())
        }))
        .and_then(|found| found.ok_or_else(|| fs.err(Errcode::ENOENT_inode)));

    crate::bch_err_msg!(fs, ret, "fetching inode {inum}")
}

/// Delete the dirent at @pos, as fsck repair: as bch2_fsck_remove_dirent().
pub fn fsck_remove(t: &TransAttempt<'_, '_>, pos: Bpos) -> Result<(), BchError> {
    let dir = lookup_first_inode(t, pos.inode)?;
    let hash_info = str_hash::hash_info_init_unchecked(t.fs(), &dir);

    let mut iter = BtreeIter::new(t, Btree::dirents, pos, BtreeIterFlags::INTENT);
    iter.traverse(t)?;
    str_hash::delete_at::<Dirents>(t, &hash_info, &mut iter,
                                   UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}
