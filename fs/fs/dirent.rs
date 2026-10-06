// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::BkeySC;
use crate::btree::bkey_methods::{self, SetError};
use crate::btree::iter::{
    BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt, TransBkey, UpdateTriggerFlags,
};
use crate::c;
use crate::errcode::{self, ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::str_hash::HashTable;
use crate::typeinfo::AccessError;
use crate::util::os_str::{qstr, qstr_name, OsStr, OsStrExt};
use core::ffi::c_void;
use core::fmt;
use core::mem::size_of;

/// The dirents btree, as a hash table.
pub struct Dirents;

/// Keys are looked up by the name they're hashed by - casefolded, in a
/// casefolded directory. The hash and compare functions are still dirent.c's:
/// its table's callbacks. C's cmp callbacks answer the opposite of a match.
impl HashTable for Dirents {
    type Key<'k> = OsStr;

    const BTREE:    c::btree_id      = c::btree_id::dirents;
    const KEY_TYPE: c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_dirent;

    fn desc() -> &'static c::bch_hash_desc {
        unsafe { &c::bch2_dirent_hash_desc }
    }

    fn hash_key(info: &c::bch_hash_info, key: &OsStr) -> u64 {
        let key = qstr(key);
        let key = &key as *const c::qstr as *const c_void;
        unsafe { (Self::desc().hash_key.expect("hash_key"))(info, key) }
    }

    fn hash_bkey(info: &c::bch_hash_info, k: BkeySC<'_>) -> u64 {
        unsafe { (Self::desc().hash_bkey.expect("hash_bkey"))(info, k.to_raw()) }
    }

    fn matches(k: BkeySC<'_>, key: &OsStr) -> bool {
        let key = qstr(key);
        let key = &key as *const c::qstr as *const c_void;
        unsafe { !(Self::desc().cmp_key.expect("cmp_key"))(k.to_raw(), key) }
    }

    fn same_name(a: BkeySC<'_>, b: BkeySC<'_>) -> bool {
        unsafe { !(Self::desc().cmp_bkey.expect("cmp_bkey"))(a.to_raw(), b.to_raw()) }
    }

    fn is_visible(inum: c::subvol_inum, k: BkeySC<'_>) -> bool {
        unsafe { (Self::desc().is_visible.expect("is_visible"))(inum, k.to_raw()) }
    }
}

/// How @k, a dirent, fails to match @inode, which it was expected to point
/// at - for formatting with {}.
pub fn inode_mismatch<'a, 'k>(
    fs:    &'a Fs,
    k:     BkeySC<'k>,
    inode: &'a c::bch_inode_unpacked,
) -> InodeMismatch<'a, 'k> {
    InodeMismatch { fs, k, inode }
}

pub struct InodeMismatch<'a, 'k> {
    fs:    &'a Fs,
    k:     BkeySC<'k>,
    inode: &'a c::bch_inode_unpacked,
}

impl fmt::Display for InodeMismatch<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "inode points to dirent that does not point back:\n{}\n{}",
               self.k.to_text(self.fs), self.inode)
    }
}

/// Delete the dirent at @pos, as fsck repair: as bch2_fsck_remove_dirent().
pub fn fsck_remove(trans: &BtreeTrans<'_>, pos: c::bpos) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_fsck_remove_dirent(trans.raw(), pos) })
}

/// Check that @k, a dirent at @iter, and @target, the inode it points at,
/// agree - the inode's backpointer and the dirent's d_type - repairing
/// whichever is wrong, as fsck: as bch2_check_dirent_target().
pub fn check_target(
    trans:  &BtreeTrans<'_>,
    iter:   &BtreeIter<'_>,
    k:      BkeySC<'_>,
    target: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    let d = k.to_c_dirent().expect("a dirent");
    ret_to_result(unsafe { c::bch2_check_dirent_target(trans.raw(), iter.raw(), d, target, true) })
}

/// What a dirent names, by d_type: for DT_SUBVOL, a subvolume (child) and
/// the subvolume the dirent lives in (parent); otherwise an inode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirentTarget {
    Inode(u64),
    Subvol { child: u32, parent: u32 },
}

impl c::bch_dirent {
    /// The inode number the dirent points at. Meaningless for a DT_SUBVOL
    /// dirent, which keeps subvolume IDs in the same space.
    pub fn d_inum(&self) -> u64 {
        u64::from_le(unsafe { self.__bindgen_anon_1.d_inum })
    }

    /// The target, read through the half of the union d_type says is live.
    pub fn target(&self) -> DirentTarget {
        if self.d_type() as u32 == c::DT_SUBVOL {
            let s = unsafe { self.__bindgen_anon_1.__bindgen_anon_1 };
            DirentTarget::Subvol {
                child:  u32::from_le(s.d_child_subvol),
                parent: u32::from_le(s.d_parent_subvol),
            }
        } else {
            DirentTarget::Inode(self.d_inum())
        }
    }

    /// Set the subvolume a DT_SUBVOL dirent lives in.
    pub fn set_parent_subvol(&mut self, subvol: u32) {
        assert_eq!(self.d_type() as u32, c::DT_SUBVOL);
        self.__bindgen_anon_1.__bindgen_anon_1.d_parent_subvol = subvol.to_le();
    }
}

/// Whether @d names @inode - a DT_SUBVOL dirent by subvolume, any other by
/// inode number: as dirent_points_to_inode_nowarn().
pub fn points_to_inode(d: &c::bch_dirent, inode: &c::bch_inode_unpacked) -> bool {
    match d.target() {
        DirentTarget::Subvol { child, .. } => child == inode.bi_subvol,
        DirentTarget::Inode(inum)          => inum == inode.bi_inum,
    }
}

/// Whether directory @dir is empty as seen in @snapshot - in subvolume
/// @subvol, or 0 to see subvolume dirents too: as bch2_empty_dir_snapshot().
/// ENOTEMPTY_dir_not_empty if it isn't.
pub fn empty_dir_snapshot(
    trans:    &BtreeTrans<'_>,
    dir:      u64,
    subvol:   u32,
    snapshot: u32,
) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_empty_dir_snapshot(trans.raw(), dir, subvol, snapshot) })
}

/// @k's name - @k a dirent: as bch2_dirent_get_name().
pub fn name<'k>(k: BkeySC<'k>) -> &'k OsStr {
    let q = unsafe { c::bch2_dirent_get_name(k.to_c_dirent().expect("a dirent")) };
    // The name is in @k's value:
    unsafe { qstr_name(&q) }
}

/// A dirent key at @pos with room for any name, to be filled in with
/// copy_target() and init_name().
pub fn alloc_max<'a, 't>(t: &TransAttempt<'a, 't>, pos: c::bpos)
    -> Result<TransBkey<'a, 't>, BchError>
{
    const BKEY_U64S: usize = size_of::<c::bkey>() / size_of::<u64>();
    t.bkey_alloc_init(u8::MAX as usize - BKEY_U64S,
                      c::bch_bkey_type::KEY_TYPE_dirent.0 as u8, pos)
}

/// Point @dst, a dirent, at what @src points at: as dirent_copy_target().
pub fn copy_target(dst: &mut TransBkey<'_, '_>, src: BkeySC<'_>) {
    unsafe {
        c::dirent_copy_target(dst.k_i_mut() as *mut c::bkey_i as *mut c::bkey_i_dirent,
                              src.to_c_dirent().expect("a dirent"))
    }
}

/// Give @new, a dirent, the name @name - hashed under @hash_info, which
/// says whether to casefold it - and size it to fit: as
/// bch2_dirent_init_name().
pub fn init_name(
    fs:        &Fs,
    new:       &mut TransBkey<'_, '_>,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
) -> Result<(), BchError> {
    let name = qstr(name);
    ret_to_result(unsafe {
        c::bch2_dirent_init_name(fs.raw,
                                 new.k_i_mut() as *mut c::bkey_i as *mut c::bkey_i_dirent,
                                 hash_info, &name, core::ptr::null())
    })
}

/// Set @field of dirent @k to @val, from text, for TransBkey::set(): see
/// btree/bkey_methods.rs.
///
/// The target is a union tagged by d_type - d_inum, or (d_child_subvol,
/// d_parent_subvol) for DT_SUBVOL - and d_type is a C bitfield: typeinfo
/// reaches neither. Each of these writes exactly the field named, tag or not,
/// so a test can make them disagree. d_name rewrites the name, not casefolded,
/// and leaves the key where it is - at the old name's offset, which is the
/// point: a key at the wrong offset, or a duplicate of another name.
pub fn set_field<'p>(k: &mut TransBkey<'_, '_>, fs: &Fs, field: &'p str, val: &'p str)
    -> Result<(), SetError<'p>>
{
    let overflow = |bytes, val| SetError::Access {
        field,
        err: AccessError::Overflow { bytes, val },
    };

    if field == "d_name" {
        return set_name(k, fs, val);
    }

    if !matches!(field, "d_type" | "d_inum" | "d_child_subvol" | "d_parent_subvol") {
        return k.set_fixed(field, val);
    }

    let v = bkey_methods::parse_val(k.k().type_, field, val)?;
    let d = unsafe { &mut *(&mut k.k_i_mut().v as *mut c::bch_val as *mut c::bch_dirent) };

    match field {
        "d_type" => {
            if v >= 1 << 5 {
                return Err(SetError::Access {
                    field,
                    err: AccessError::BitsOverflow { bits: 5, val: v },
                });
            }
            d.set_d_type(v as u8);
        }
        "d_inum" => d.__bindgen_anon_1.d_inum = v.to_le(),
        _ => {
            let v = u32::try_from(v).map_err(|_| overflow(4, v))?.to_le();
            let s = unsafe { &mut d.__bindgen_anon_1.__bindgen_anon_1 };
            if field == "d_child_subvol" {
                s.d_child_subvol = v;
            } else {
                s.d_parent_subvol = v;
            }
        }
    }
    Ok(())
}

fn set_name<'p>(k: &mut TransBkey<'_, '_>, fs: &Fs, val: &'p str) -> Result<(), SetError<'p>> {
    if val.is_empty() || val.len() > c::BCH_NAME_MAX as usize {
        return Err(SetError::BadName { val });
    }

    // init_name() sizes the value down to the name, from whatever room the
    // key claims:
    let have_u64s = k.as_u64s().len();
    k.k_mut().u64s = have_u64s.min(u8::MAX as usize) as u8;

    // Only cf_encoding is read, and NULL means not casefolded:
    let hash_info: c::bch_hash_info = unsafe { core::mem::zeroed() };
    init_name(fs, k, &hash_info, OsStr::from_bytes(val.as_bytes()))
        .map_err(|_| SetError::BadName { val })
}

/// Create dirent @name in directory @dir, in subvolume @subvol at @snapshot,
/// pointing at @target: as bch2_dirent_create_snapshot(). Where it went is
/// returned in @dir_offset.
#[allow(clippy::too_many_arguments)]
pub fn create_snapshot(
    t:            &TransAttempt<'_, '_>,
    subvol:       u32,
    snapshot:     u32,
    dir:          &mut c::bch_inode_unpacked,
    d_type:       u8,
    name:         &OsStr,
    target:       DirentTarget,
    dir_offset:   &mut u64,
    iter_flags:   BtreeIterFlags,
    update_flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    // The C takes the subvolume or inode number, and gets which from @d_type;
    // a subvolume dirent's parent is @subvol.
    let target = match target {
        DirentTarget::Subvol { child, .. } => child as u64,
        DirentTarget::Inode(inum)          => inum,
    };
    let name = qstr(name);
    let ret = unsafe {
        c::bch2_dirent_create_snapshot(t.raw(), subvol, snapshot, dir, d_type, &name, target,
                                       dir_offset,
                                       c::btree_iter_update_trigger_flags(iter_flags.bits() |
                                                                          update_flags.bits()))
    };
    t.result(ret)
}

/// A new dirent in directory @dir named @name, of type @d_type, pointing at
/// @target - an inode, or for DT_SUBVOL a subvolume: as
/// bch2_dirent_create_key(). Its position is for the caller to fill in.
pub fn create_key<'a, 't>(
    t:         &TransAttempt<'a, 't>,
    hash_info: &c::bch_hash_info,
    dir:       c::subvol_inum,
    d_type:    u8,
    name:      &OsStr,
    target:    u64,
) -> Result<TransBkey<'a, 't>, BchError> {
    let name = qstr(name);
    unsafe {
        let k = c::bch2_dirent_create_key(t.raw(), hash_info, dir, d_type, &name,
                                          core::ptr::null(), target);
        TransBkey::from_raw(t, k as *mut c::bkey_i)
    }
}

pub fn lookup(
    fs:        &Fs,
    dir_inum:  c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
) -> Result<c::subvol_inum, BchError> {
    let mut inum: c::subvol_inum = Default::default();
    let ret = unsafe {
        c::bch2_dirent_lookup(fs.raw, dir_inum, hash_info, &qstr(name), &mut inum)
    };
    errcode::ret_to_result(ret as i32)?;
    Ok(inum)
}

pub fn readdir(
    fs:        &Fs,
    dir_inum:  c::subvol_inum,
    hash_info: &mut c::bch_hash_info,
    ctx:       &mut c::dir_context,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_readdir(fs.raw, dir_inum, hash_info, ctx)
    })
}
