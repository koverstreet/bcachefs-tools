// SPDX-License-Identifier: GPL-2.0

//! Compat: metadata as an older version, or a machine of the other
//! endianness, wrote it - made this version's and this machine's on read, and
//! back on write.
//!
//! Each transform is its own inverse, or paired with its inverse by @write,
//! so read then write is the identity. The rest of the compat code -
//! compat_bformat(), compat_btree_node(), __bch2_bkey_compat() - is C's.
//!
//! Every transform is of metadata not yet validated: the key-level entry
//! points, ->swab and ->compat, should take Unvalidated keys. XXX: they don't
//! yet - Rust has no unvalidated keys to give them.

use crate::c;

impl c::bpos {
    /// Byte-swapped, as written by a machine of the other endianness: every
    /// byte reversed - a bpos's fields are in reverse order on big endian, so
    /// that's each field swapped and the order flipped. bch2_bpos_swab().
    pub fn swab(&mut self) {
        // SAFETY: a bpos is Plain - every byte part of a field, any bytes a bpos
        let bytes = unsafe {
            core::slice::from_raw_parts_mut((self as *mut Self).cast::<u8>(), size_of::<Self>())
        };
        bytes.reverse();
    }

    /// A position, in @btree, as @version on a machine @big_endian or not
    /// wrote it, made this one's - or this one's made that, to write: it's
    /// its own inverse. compat_bpos().
    pub fn compat(&mut self, btree: c::btree_id, version: c::bcachefs_metadata_version,
                  big_endian: bool) {
        if big_endian != cfg!(target_endian = "big") {
            self.swab();
        }

        if version < c::bcachefs_metadata_version::inode_btree_change &&
           btree == c::btree_id::BTREE_ID_inodes {
            (self.inode, self.offset) = (self.offset, self.inode);
        }
    }
}
