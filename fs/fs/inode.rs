// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, CommitOpts, TransAttempt, TransError};
use crate::{btree, btree_id, printbuf_to_formatter};
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

/// A subvolume's root: deleting it is subvolume deletion's job.
pub fn is_subvolume_root(inode: &c::bch_inode_unpacked) -> bool {
    unsafe { c::bch2_inode_is_subvolume_root(inode) }
}

/// Whether @k is an inode, of any version: as C's bkey_is_inode().
pub fn bkey_is_inode(k: &c::bkey) -> bool {
    unsafe { c::bkey_is_inode(k) }
}

/// Unpack @k, an inode of any version (see bkey_is_inode()): as
/// bch2_inode_unpack().
pub fn unpack(fs: &Fs, k: BkeySC<'_>) -> c::bch_inode_unpacked {
    let mut u: c::bch_inode_unpacked = Default::default();
    unsafe { c::bch2_inode_unpack(fs.raw, k.to_raw(), &mut u) };
    u
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
    t:     TransAttempt<'a, 't>,
    iter:  &mut BtreeIter<'t>,
    inode: &mut c::bch_inode_unpacked,
    inum:  c::subvol_inum,
    flags: BtreeIterFlags,
) -> Result<TransAttempt<'a, 't>, TransError> {
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
    t:     TransAttempt<'a, 't>,
    iter:  &mut BtreeIter<'t>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<TransAttempt<'a, 't>, TransError> {
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
            CommitOpts::new(),
            BtreeIterFlags::CACHED,
        )
    }
}
