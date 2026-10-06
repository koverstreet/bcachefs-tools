// SPDX-License-Identifier: GPL-2.0

use core::ffi::{c_void, CStr};

use crate::btree::iter::TransAttempt;
use crate::c;
use crate::errcode::BchError;
use crate::btree::bkey::BkeySC;
use crate::str_hash::{self, HashTable};
use crate::util::os_str::{OsStr, OsStrExt};

/// The xattrs btree, as a hash table.
pub struct Xattrs;

/// Keys are a type and a name.
impl HashTable for Xattrs {
    type Key<'k> = XattrKey<'k>;

    const BTREE:    c::btree_id      = c::btree_id::xattrs;
    const KEY_TYPE: c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_xattr;

    fn desc() -> &'static c::bch_hash_desc {
        unsafe { &c::bch2_xattr_hash_desc }
    }

    fn hash_key(info: &c::bch_hash_info, key: &XattrKey<'_>) -> u64 {
        hash(info, key.type_, key.name.as_bytes())
    }

    fn hash_bkey(info: &c::bch_hash_info, k: BkeySC<'_>) -> u64 {
        let (type_, name) = type_and_name(k);
        hash(info, type_, name)
    }

    fn matches(k: BkeySC<'_>, key: &XattrKey<'_>) -> bool {
        type_and_name(k) == (key.type_, key.name.as_bytes())
    }

    fn same_name(a: BkeySC<'_>, b: BkeySC<'_>) -> bool {
        type_and_name(a) == type_and_name(b)
    }
}

/// An xattr's hash under @info: its type, then its name - as
/// bch2_xattr_hash().
fn hash(info: &c::bch_hash_info, type_: u8, name: &[u8]) -> u64 {
    str_hash::hash_parts(info, &[&[type_], name], false)
}

/// What an xattr lookup searches for: C's struct xattr_search_key.
pub struct XattrKey<'k> {
    pub type_: u8,
    pub name:  &'k OsStr,
}

/// A type and name, to look up: C's X_SEARCH().
pub fn search_key(type_: u32, name: &OsStr) -> XattrKey<'_> {
    XattrKey { type_: type_ as u8, name }
}

/// @k's type and name - @k an xattr, valid.
fn type_and_name(k: BkeySC<'_>) -> (u8, &[u8]) {
    let x = k.as_xattr().expect("an xattr");
    let start = core::mem::offset_of!(c::bch_xattr, x_name_and_value);
    (x.x_type, &k.val_bytes()[start..][..x.x_name_len as usize])
}

pub fn set(
    t:     &TransAttempt<'_, '_>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  &CStr,
    val:   &[u8],
    typ:   i32,
    flags: i32,
) -> Result<(), BchError> {
    let ret = unsafe {
        c::bch2_xattr_set(
            t.raw(),
            inum,
            inode,
            name.as_ptr(),
            val.as_ptr() as *const c_void,
            val.len(),
            typ,
            flags,
        )
    };
    t.result(ret)
}
