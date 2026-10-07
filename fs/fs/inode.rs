// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::errcode::{self, ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::dirent::DirentTarget;
use crate::btree::bkey::BkeySC;
use crate::btree::iter::{
    bkey_s_c_to_result, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt,
    UpdateTriggerFlags,
};
use crate::{btree, btree_id, printbuf_to_formatter};
use core::ffi::CStr;
use core::fmt;

pub fn find_by_inum(
    fs:   &Fs,
    inum: c::subvol_inum,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut inode: c::bch_inode_unpacked = Default::default();
    ret_to_result(unsafe {
        c::bch2_inode_find_by_inum(fs.raw, inum, &mut inode)
    })?;
    Ok(inode)
}

/// The inode @inum, in a transaction: as bch2_inode_find_by_inum_trans(). On
/// error, C reports it, naming @warn - pass c_function_name!(), as C passes
/// __func__.
pub fn find_by_inum_trans(
    trans: &BtreeTrans<'_>,
    inum:  c::subvol_inum,
    warn:  &CStr,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut inode = c::bch_inode_unpacked::default();
    ret_to_result(unsafe {
        c::__bch2_inode_find_by_inum_trans(trans.raw(), inum, &mut inode, warn.as_ptr())
    })?;
    Ok(inode)
}

/// Inode @inum as seen in @snapshot: as bch2_inode_find_by_inum_snapshot().
/// ENOENT_inode if there's no inode there.
pub fn find_by_inum_snapshot(
    trans:    &BtreeTrans<'_>,
    inum:     u64,
    snapshot: u32,
    flags:    BtreeIterFlags,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut inode = c::bch_inode_unpacked::default();
    ret_to_result(unsafe {
        c::bch2_inode_find_by_inum_snapshot(trans.raw(), inum, snapshot, &mut inode, flags.bits())
    })?;
    Ok(inode)
}

/// A new inode, with its times set to now: as bch2_inode_init(). @parent's
/// inheritable options are inherited.
pub fn init(
    fs:     &Fs,
    uid:    c::uid_t,
    gid:    c::gid_t,
    mode:   c::umode_t,
    rdev:   c::dev_t,
    parent: Option<&c::bch_inode_unpacked>,
) -> c::bch_inode_unpacked {
    let mut inode = c::bch_inode_unpacked::default();
    let parent = parent.map_or(core::ptr::null_mut(), |p| p as *const _ as *mut _);
    unsafe { c::bch2_inode_init(fs.raw, &mut inode, uid, gid, mode, rdev, parent) };
    inode
}

pub fn init_early(fs: &Fs, inode: &mut c::bch_inode_unpacked) {
    unsafe { c::bch2_inode_init_early(fs.raw, inode) };
}

pub fn opts_get_inode(fs: &Fs, inode: &c::bch_inode_unpacked) -> c::bch_inode_opts {
    let mut opts: c::bch_inode_opts = Default::default();
    unsafe {
        c::bch2_inode_opts_get_inode(fs.raw, inode as *const _ as *mut _, &mut opts);
    }
    opts
}

pub fn rm(fs: &Fs, inum: c::subvol_inum) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_inode_rm(fs.raw, inum) })
}

/// The unicode map directory @inode's names are casefolded with, or NULL if
/// it isn't casefolded: for its bch_hash_info.
pub fn cf_encoding(fs: &Fs, inode: &c::bch_inode_unpacked) -> *mut c::unicode_map {
    if unsafe { c::bch2_inode_casefold(fs.raw, inode) } {
        unsafe { (*fs.raw).cf_encoding }
    } else {
        core::ptr::null_mut()
    }
}

/// A subvolume's root: deleting it is subvolume deletion's job.
pub fn is_subvolume_root(inode: &c::bch_inode_unpacked) -> bool {
    unsafe { c::bch2_inode_is_subvolume_root(inode) }
}

/// Whether @k is an inode, of any version: as C's bkey_is_inode().
pub fn bkey_is_inode(k: &c::bkey) -> bool {
    unsafe { c::bkey_is_inode(k) }
}

/// @k's mode, without unpacking it - an inode of any version: as C's
/// bkey_inode_mode().
pub fn mode(k: BkeySC<'_>) -> u32 {
    unsafe { c::bkey_inode_mode(k.to_raw()) }
}

/// @k's flags (bch_inode_flags bits), without unpacking it - an inode of any
/// version: as bch2_inode_flags().
pub fn flags(k: BkeySC<'_>) -> u32 {
    unsafe { c::bch2_inode_flags(k.to_raw()) }
}

/// Whether @inode records the dirent naming it (bi_dir, bi_dir_offset): as
/// bch2_inode_has_backpointer().
pub fn has_backpointer(inode: &c::bch_inode_unpacked) -> bool {
    unsafe { c::bch2_inode_has_backpointer(inode) }
}

/// The dirent naming @inode, looked up through @iter: as
/// bch2_inode_get_dirent(). @snapshot is where to look, and on return where
/// the lookup looked - a subvolume root is named in its parent subvolume.
pub fn get_dirent<'i>(
    trans:    &BtreeTrans<'_>,
    iter:     &'i mut BtreeIter<'_>,
    inode:    &c::bch_inode_unpacked,
    snapshot: &mut u32,
) -> Result<BkeySC<'i>, BchError> {
    // 'i: the dirent is valid while @iter, which it came through, is borrowed
    let d = unsafe {
        let d = c::bch2_inode_get_dirent(trans.raw(), iter.raw_mut(), inode as *const _ as *mut _,
                                         snapshot);
        bkey_s_c_to_result(d.into())
    }?;
    Ok(d.expect("a dirent lookup returns a key or an error"))
}

/// Some version of @inum, in any snapshot: as bch2_inode_find_any_snapshot().
pub fn find_any_snapshot(trans: &BtreeTrans<'_>, inum: u64) -> Result<c::bch_inode_unpacked, BchError> {
    let mut inode = c::bch_inode_unpacked::default();
    ret_to_result(unsafe { c::bch2_inode_find_any_snapshot(trans.raw(), inum, &mut inode) })?;
    Ok(inode)
}

/// Give @inode a free inode number in @snapshot - below 2^32, with @is_32bit -
/// and queue its creation through @iter: as bch2_inode_create().
pub fn create<'a, 't>(
    t:        &TransAttempt<'a, 't>,
    iter:     &mut BtreeIter<'t>,
    inode:    &mut c::bch_inode_unpacked,
    snapshot: u32,
    is_32bit: bool,
) -> Result<(), BchError> {
    let ret = unsafe { c::bch2_inode_create(t.raw(), iter.raw_mut(), inode, snapshot, is_32bit) };
    t.result(ret)
}

/// As write(), with update flags: as bch2_inode_write_flags().
pub fn write_flags<'a, 't>(
    t:     &TransAttempt<'a, 't>,
    iter:  &mut BtreeIter<'t>,
    inode: &mut c::bch_inode_unpacked,
    flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    let ret = unsafe {
        c::bch2_inode_write_flags(t.raw(), iter.raw_mut(), inode,
                                  c::btree_iter_update_trigger_flags(flags.bits()))
    };
    t.result(ret)
}

/// Turn casefolding on for directory @inode, @inum, to @v: it has to be
/// empty, and the filesystem able to casefold - as bch2_inode_set_casefold().
pub fn set_casefold(
    t:     &TransAttempt<'_, '_>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    v:     u32,
) -> Result<(), BchError> {
    let ret = unsafe { c::bch2_inode_set_casefold(t.raw(), inum, inode, v) };
    t.result(ret)
}

/// The oldest version of @inum that a key in @snapshot sees - the version
/// all the others take their hash info from: as
/// bch2_inode_find_oldest_snapshot().
pub fn find_oldest_snapshot(
    trans:    &BtreeTrans<'_>,
    inum:     u64,
    snapshot: u32,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut root = c::bch_inode_unpacked::default();
    ret_to_result(unsafe {
        c::bch2_inode_find_oldest_snapshot(trans.raw(), inum, snapshot, &mut root)
    })?;
    Ok(root)
}

/// Whether the inode at @pos has versions in descendant snapshots: as
/// bch2_inode_has_child_snapshots().
pub fn has_child_snapshots(trans: &BtreeTrans<'_>, pos: c::bpos) -> Result<bool, BchError> {
    Ok(errcode::ret_to_result(unsafe { c::bch2_inode_has_child_snapshots(trans.raw(), pos) })? != 0)
}

/// Whether the inode at @pos, or a version of it in a descendant snapshot,
/// is open in the VFS: as bch2_inode_or_descendents_is_open(). Never, in
/// userspace.
pub fn or_descendents_is_open(trans: &BtreeTrans<'_>, pos: c::bpos) -> Result<bool, BchError> {
    Ok(errcode::ret_to_result(unsafe { c::rust_inode_or_descendents_is_open(trans.raw(), pos) })? != 0)
}

/// Delete inode @inum in @snapshot, and everything it owns there: as
/// bch2_inode_rm_snapshot().
pub fn rm_snapshot(trans: &BtreeTrans<'_>, inum: u64, snapshot: u32) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_inode_rm_snapshot(trans.raw(), inum, snapshot) })
}

/// Check that @inode's options have been propagated to its descendants,
/// repairing it if not: as bch2_check_inode_opts_propagated().
pub fn check_opts_propagated(
    trans: &BtreeTrans<'_>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    ret_to_result(unsafe { c::bch2_check_inode_opts_propagated(trans.raw(), inode) })
}

/// Unpack @k, an inode of any version (see bkey_is_inode()): as
/// bch2_inode_unpack().
pub fn unpack(fs: &Fs, k: BkeySC<'_>) -> c::bch_inode_unpacked {
    let mut u: c::bch_inode_unpacked = Default::default();
    unsafe { c::bch2_inode_unpack(fs.raw, k.to_raw(), &mut u) };
    u
}

impl c::bch_inode_unpacked {
    /// The link count as the VFS counts it - bi_nlink leaves out the links
    /// every inode has, two for a directory - and 0 if it's unlinked: as
    /// bch2_inode_nlink_get().
    pub fn nlink(&self) -> u32 {
        unsafe { c::bch2_inode_nlink_get(self as *const _ as *mut _) }
    }

    /// Set the link count, as nlink() counts it; 0 marks the inode unlinked:
    /// as bch2_inode_nlink_set().
    pub fn set_nlink(&mut self, nlink: u32) {
        unsafe { c::bch2_inode_nlink_set(self, nlink) }
    }

    /// Whether flag @f (BCH_INODE_*) is set.
    pub fn flag(&self, f: c::bch_inode_flags) -> bool {
        self.bi_flags & f as u32 != 0
    }

    pub fn set_flag(&mut self, f: c::bch_inode_flags, v: bool) {
        if v {
            self.bi_flags |= f as u32;
        } else {
            self.bi_flags &= !(f as u32);
        }
    }

    /// Forget the dirent naming this inode: the backpointer is the pair
    /// (bi_dir, bi_dir_offset), cleared together or not at all.
    pub fn clear_backpointer(&mut self) {
        self.bi_dir        = 0;
        self.bi_dir_offset = 0;
    }

    pub fn is_dir(&self) -> bool {
        self.bi_mode as u32 & c::S_IFMT == c::S_IFDIR
    }

    /// The dirent type naming it: as inode_d_type().
    pub fn d_type(&self) -> u8 {
        if self.bi_subvol != 0 {
            c::DT_SUBVOL as u8
        } else {
            ((self.bi_mode >> 12) & 15) as u8
        }
    }

    /// What a dirent naming it points at: a subvolume root by subvolume,
    /// linked from its parent subvolume; anything else by inode number.
    pub fn dirent_target(&self) -> DirentTarget {
        if self.bi_subvol != 0 {
            DirentTarget::Subvol { child: self.bi_subvol, parent: self.bi_parent_subvol }
        } else {
            DirentTarget::Inode(self.bi_inum)
        }
    }

    /// Whether it counts towards its parent's link count: a directory, but not
    /// a subvolume root, which is named by a DT_SUBVOL dirent: as
    /// is_subdir_for_nlink().
    pub fn is_subdir_for_nlink(&self) -> bool {
        self.is_dir() && self.bi_subvol == 0
    }

    /// The str_hash type its dirents or xattrs are hashed with: C's
    /// INODE_STR_HASH().
    pub fn str_hash(&self) -> u64 {
        unsafe { c::INODE_STR_HASH(self) }
    }

    pub fn set_str_hash(&mut self, v: u64) {
        unsafe { c::SET_INODE_STR_HASH(self, v) }
    }

    /// Whether any per-inode option is set - what BCH_INODE_has_inode_opts
    /// records: as bch2_inode_has_opts().
    pub fn has_opts(&self) -> bool {
        unsafe { c::bch2_inode_has_opts(self) }
    }
}

/// Queue writing back @inode, for fsck repair - the caller commits: as
/// __bch2_fsck_write_inode().
pub fn fsck_write<'a, 't>(
    t:     &TransAttempt<'a, 't>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    let ret = unsafe { c::__bch2_fsck_write_inode(t.raw(), inode) };
    t.result(ret)
}

impl fmt::Display for c::bch_inode_unpacked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe { c::bch2_inode_unpacked_to_text(buf, self) })
    }
}

pub fn fsck_write_inode(
    trans: &btree::iter::BtreeTrans<'_>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    ret_to_result(unsafe {
        c::bch2_fsck_write_inode(trans.raw(), inode)
    })
}

pub fn peek<'a, 't>(
    t:     &TransAttempt<'a, 't>,
    iter:  &mut BtreeIter<'t>,
    inode: &mut c::bch_inode_unpacked,
    inum:  c::subvol_inum,
    flags: BtreeIterFlags,
) -> Result<(), BchError> {
    let ret = unsafe {
        c::__bch2_inode_peek(
            t.raw(),
            iter.raw_mut(),
            inode,
            inum,
            flags.bits(),
            core::ptr::null(),
        )
    };
    t.result(ret)
}

pub fn write<'a, 't>(
    t:     &TransAttempt<'a, 't>,
    iter:  &mut BtreeIter<'t>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    let ret = unsafe {
        c::bch2_inode_write(t.raw(), iter.raw_mut(), inode)
    };
    t.result(ret)
}

pub fn write_cached(fs: &Fs, inode: &c::bch_inode_unpacked) -> Result<(), BchError> {
    unsafe {
        let mut packed: c::bkey_inode_buf = Default::default();
        c::bch2_inode_pack(fs.raw, &mut packed, inode);
        packed.inode.__bindgen_anon_1.k.as_mut().p.snapshot = u32::MAX;
        fs.btree_insert(
            btree_id::inodes,
            packed.inode.__bindgen_anon_1.k_i.as_mut(),
            None,
            CommitFlags::empty(),
            BtreeIterFlags::CACHED,
        )
    }
}
