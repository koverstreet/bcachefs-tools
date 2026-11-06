// SPDX-License-Identifier: GPL-2.0

//! Compat: metadata as an older version, or a machine of the other
//! endianness, wrote it - made this version's and this machine's on read, and
//! back on write.
//!
//! Each transform is its own inverse, or paired with its inverse by @write,
//! so read then write is the identity. The rest of the compat code -
//! compat_bformat(), compat_btree_node() - is C's.
//!
//! Every transform is of metadata not yet validated: the key-level entry
//! points, ->swab and ->compat, should take Unvalidated keys. XXX: they don't
//! yet - Rust has no unvalidated keys to give them.

use crate::btree::bkey::{BkeyP, BkeyS};
use crate::c;
use crate::fs::Fs;
use crate::format::{Bpos, Btree};
use core::ffi::{c_int, c_uint};

impl Bpos {
    /// Byte-swapped, as written by a machine of the other endianness: every
    /// byte reversed - a bpos's fields are in reverse order on big endian, so
    /// that's each field swapped and the order flipped. bch2_bpos_swab().
    pub fn swab(&mut self) {
        crate::util::vstructs::plain_bytes_mut(self).reverse();
    }

    /// A position, in @btree, as @version on a machine @big_endian or not
    /// wrote it, made this one's - or this one's made that, to write: it's
    /// its own inverse. compat_bpos().
    pub fn compat(&mut self, btree: Btree, version: c::bcachefs_metadata_version,
                  big_endian: bool) {
        if big_endian != cfg!(target_endian = "big") {
            self.swab();
        }

        if version < c::bcachefs_metadata_version::inode_btree_change &&
           btree == Btree::BTREE_ID_inodes {
            (self.inode, self.offset) = (self.offset, self.inode);
        }
    }
}

/// Key types the bkey_renumber version renumbered, by btree node type: the old
/// number, and the new.
const RENUMBER: [(c::btree_node_type, u8, c::bch_bkey_type); 12] = {
    use c::bch_bkey_type as t;
    use c::btree_node_type as n;
    [
        (n::BKEY_TYPE_btree,   128, t::KEY_TYPE_btree_ptr),
        (n::BKEY_TYPE_extents, 128, t::KEY_TYPE_extent),
        (n::BKEY_TYPE_extents, 129, t::KEY_TYPE_extent),
        (n::BKEY_TYPE_extents, 130, t::KEY_TYPE_reservation),
        (n::BKEY_TYPE_inodes,  128, t::KEY_TYPE_inode),
        (n::BKEY_TYPE_inodes,  130, t::KEY_TYPE_inode_generation),
        (n::BKEY_TYPE_dirents, 128, t::KEY_TYPE_dirent),
        (n::BKEY_TYPE_dirents, 129, t::KEY_TYPE_hash_whiteout),
        (n::BKEY_TYPE_xattrs,  128, t::KEY_TYPE_xattr),
        (n::BKEY_TYPE_xattrs,  129, t::KEY_TYPE_hash_whiteout),
        (n::BKEY_TYPE_alloc,   128, t::KEY_TYPE_alloc),
        (n::BKEY_TYPE_quotas,  128, t::KEY_TYPE_quota),
    ]
};

/// bch2_bkey_renumber(): @k's type, in a node of @node_type, to the old
/// numbering to write, or from it read.
fn renumber(node_type: c::btree_node_type, k: &mut c::bkey_packed, write: bool) {
    let found = RENUMBER.iter().find(|&&(n, old, new)| {
        n == node_type && k.type_ == if write { new.0 as u8 } else { old }
    });
    if let Some(&(_, old, new)) = found {
        k.type_ = if write { old } else { new.0 as u8 };
    }
}

/// __btree_node_type().
fn node_type(level: u32, btree: Btree) -> c::btree_node_type {
    if level != 0 {
        c::btree_node_type::BKEY_TYPE_btree
    } else {
        c::btree_node_type::from_bits(btree.0 + 1)
    }
}

/// __bch2_bkey_compat(): @k, in a node at @level of @btree that @version on
/// a machine @big_endian or not wrote, made this version's and this
/// machine's to read, or back to write: each step in reverse to write.
///
/// # Safety
/// @k is a key in @f's node, as long as its u64s says - not yet validated:
/// too short to be a key, it's left for validate.
#[cstruct_macros::c_export]
pub unsafe fn __bch2_bkey_compat(fs: Option<&Fs>, level: c_uint, btree: Btree, version: c_uint,
                                 big_endian: c_uint, write: c_int, f: Option<&c::bkey_format>,
                                 k: *mut c::bkey_packed) {
    use c::bch_bkey_fields::{BKEY_FIELD_INODE, BKEY_FIELD_OFFSET, BKEY_FIELD_SNAPSHOT};

    let (big_endian, write) = (big_endian != 0, write != 0);
    let version = c::bcachefs_metadata_version(version);
    let packed = |k: *mut c::bkey_packed| unsafe { (*BkeyP::from_raw(k)).is_packed() };
    // a journal key's has none: it's never packed
    let fmt = || f.expect("a packed key, with no format");

    /* on read, before it's validated: too short to be a key isn't one */
    if !unsafe { (*BkeyP::from_raw(k)).u64s_valid(f.unwrap_or(&crate::btree::bkey::BKEY_FORMAT_CURRENT)) } {
        return;
    }

    /*
     * Do these operations in reverse order in the write path:
     */
    const NR_COMPAT: u32 = 5;
    for i in 0..NR_COMPAT {
        match if write { NR_COMPAT - 1 - i } else { i } {
            0 => {
                use crate::btree::bkey::c_export::bch2_bkey_swab_key;

                if big_endian != cfg!(target_endian = "big") {
                    unsafe { bch2_bkey_swab_key(f, k) };
                } else if cfg!(CONFIG_BCACHEFS_DEBUG) {
                    unsafe { bch2_bkey_swab_key(f, k) };
                    unsafe { bch2_bkey_swab_key(f, k) };
                }
            }
            1 => {
                if version < c::bcachefs_metadata_version::bkey_renumber {
                    renumber(node_type(level, btree), unsafe { &mut *k }, write);
                }
            }
            2 => {
                if version < c::bcachefs_metadata_version::inode_btree_change &&
                   btree == Btree::BTREE_ID_inodes {
                    if !packed(k) {
                        let u = unsafe { &mut *k.cast::<c::bkey_i>() };
                        (u.k.p.inode, u.k.p.offset) = (u.k.p.offset, u.k.p.inode);
                    } else if fmt().bits(BKEY_FIELD_INODE) != 0 && fmt().bits(BKEY_FIELD_OFFSET) != 0 {
                        let mut tmp = *fmt();
                        tmp.bits_per_field.swap(BKEY_FIELD_INODE as usize, BKEY_FIELD_OFFSET as usize);
                        tmp.field_offset.swap(BKEY_FIELD_INODE as usize, BKEY_FIELD_OFFSET as usize);

                        let (src, dst) = if write { (fmt(), &tmp) } else { (&tmp, fmt()) };
                        let mut uk = src.unpack_key(unsafe { &*BkeyP::from_raw(k) });
                        (uk.p.inode, uk.p.offset) = (uk.p.offset, uk.p.inode);
                        let words = unsafe { core::slice::from_raw_parts_mut(k.cast::<u64>(), dst.key_u64s.into()) };
                        assert!(dst.pack_key(words, &uk), "inode key no longer packs");
                    }
                }
            }
            3 => {
                if version < c::bcachefs_metadata_version::snapshot &&
                   (level != 0 || btree.has_snapshots()) {
                    if !packed(k) {
                        let u = unsafe { &mut *k.cast::<c::bkey_i>() };
                        u.k.p.snapshot = if write { 0 } else { u32::MAX };
                    } else {
                        let min_packed = fmt().field_offset[BKEY_FIELD_SNAPSHOT as usize].get();
                        let bits = fmt().bits(BKEY_FIELD_SNAPSHOT);
                        let max_packed = min_packed + if bits >= 64 { u64::MAX } else { !(u64::MAX << bits) };

                        let mut uk = fmt().unpack_key(unsafe { &*BkeyP::from_raw(k) });
                        uk.p.snapshot = if write { min_packed as u32 } else { max_packed.min(u32::MAX.into()) as u32 };
                        let words = unsafe { core::slice::from_raw_parts_mut(k.cast::<u64>(), fmt().key_u64s.into()) };
                        assert!(fmt().pack_key(words, &uk), "key no longer packs");
                    }
                }
            }
            4 => {
                let mut uk;
                let u = if !packed(k) {
                    let u = unsafe { &mut *k.cast::<c::bkey_i>() };
                    unsafe { BkeyS::from_parts(&mut u.k, &mut u.v) }
                } else {
                    uk = fmt().unpack_key(unsafe { &*BkeyP::from_raw(k) });
                    let v = unsafe { k.cast::<u64>().add(fmt().key_u64s.into()) }.cast::<c::bch_val>();
                    unsafe { BkeyS::from_parts(&mut uk, v) }
                };
                let ops = crate::btree::bkey_methods::key_ops(unsafe { (*k).type_ });

                if big_endian != cfg!(target_endian = "big") {
                    let u = unsafe { BkeyS::from_parts(&mut *u.k, u.v) };
                    (ops.swab)(fs, u);
                }
                (ops.compat)(btree, version, big_endian, write, u);
            }
            _ => unreachable!(),
        }
    }
}
