// SPDX-License-Identifier: GPL-2.0

//! Compat: metadata as an older version, or a machine of the other
//! endianness, wrote it - made this version's and this machine's on read, and
//! back on write.
//!
//! Each transform is its own inverse, or paired with its inverse by @write,
//! so read then write is the identity. The rest of the compat code -
//! compat_btree_node() - is C's.
//!
//! Every transform is of metadata not yet validated: the key-level entry
//! points, ->swab and ->compat, should take Unvalidated keys. XXX: they don't
//! yet - Rust has no unvalidated keys to give them.

use crate::btree::bkey::{BkeyP, BkeyS};
use crate::c;
use crate::fs::Fs;
use crate::format::{Bpos, Btree, BkeyType, MetadataVersion};
use core::ffi::{c_int, c_uint};

impl Bpos {
    /// Byte-swapped, as written by a machine of the other endianness: every
    /// byte reversed - a bpos's fields are in reverse order on big endian, so
    /// that's each field swapped and the order flipped.
    pub fn swab(&mut self) {
        crate::util::vstructs::plain_bytes_mut(self).reverse();
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
pub unsafe fn __bch2_bkey_compat(fs: Option<&Fs>, _level: c_uint, btree: Btree, version: c_uint,
                                 big_endian: c_uint, write: c_int, f: Option<&c::bkey_format>,
                                 k: *mut c::bkey_packed) {

    let (big_endian, write) = (big_endian != 0, write != 0);
    let version = MetadataVersion(version);
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
    const NR_COMPAT: u32 = 2;
    for i in 0..NR_COMPAT {
        match if write { NR_COMPAT - 1 - i } else { i } {
            0 => {
                use crate::btree::bkey::c_export::bkey_swab_key;

                if big_endian != cfg!(target_endian = "big") {
                    unsafe { bkey_swab_key(f, k) };
                } else if cfg!(CONFIG_BCACHEFS_DEBUG) {
                    unsafe { bkey_swab_key(f, k) };
                    unsafe { bkey_swab_key(f, k) };
                }
            }
            1 => {
                let mut uk;
                let u = if !packed(k) {
                    let u = unsafe { &mut *k.cast::<c::bkey_i>() };
                    unsafe { BkeyS::from_parts(&mut u.k, &mut u.v) }
                } else {
                    uk = fmt().unpack_key(unsafe { &*BkeyP::from_raw(k) });
                    let v = unsafe { k.cast::<u64>().add(fmt().key_u64s.into()) }.cast::<c::bch_val>();
                    unsafe { BkeyS::from_parts(&mut uk, v) }
                };
                let ops = BkeyType(unsafe { (*k).type_ }.into()).ops();

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
