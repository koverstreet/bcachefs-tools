// SPDX-License-Identifier: GPL-2.0

//! Xattrs (fs/xattr.c): extended attributes - a hash table per inode in the
//! xattrs btree, keyed by type and name - and their VFS end: get, set and
//! listxattr, and the bcachefs.* namespace, which is inode options.
//!
//! The xattr_handler tables the VFS dispatches through stay C, in xattr.c,
//! pointing at the handlers here; so does what an xattr of a given type lists
//! under for a given caller, which depends on the handler.
//!
//! POSIX ACLs are xattrs too - types POSIX_ACL_ACCESS and POSIX_ACL_DEFAULT,
//! with an empty name - and are acl.rs's.

use core::ffi::{c_char, c_int, c_void};
use core::mem::{offset_of, size_of};

use crate::acl;
use crate::bkey_fsck_err_on;
use crate::btree::bkey::{BkeySC, BKEY_U64S, POS_MIN};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::inode;
use crate::snapshots::subvolume;
use crate::str_hash::{self, HashTable};
use crate::util::ffi::{bytes, opt_bytes, opt_bytes_mut, Opaque};
use crate::util::os_str::{cstr_name, OsStr, OsStrExt};
use crate::util::Printbuf;

/// setxattr() flags - <uapi/linux/xattr.h>:
pub const XATTR_CREATE:  i32 = 0x1;
pub const XATTR_REPLACE: i32 = 0x2;

// ── The hash table ───────────────────────────────────────────────────────

/// The xattrs btree, as a hash table.
pub struct Xattrs;

/// Keys are a type and a name.
impl HashTable for Xattrs {
    type Key<'k> = XattrKey<'k>;

    const BTREE:    c::btree_id      = c::btree_id::xattrs;
    const KEY_TYPE: c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_xattr;

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

// ── The value ────────────────────────────────────────────────────────────

/// Where the name starts - the value follows it.
const NAME_OFFSET: usize = offset_of!(c::bch_xattr, x_name_and_value);

/// The u64s of value an xattr with these takes: as xattr_val_u64s().
pub fn val_u64s(name_len: usize, val_len: usize) -> usize {
    (NAME_OFFSET + name_len + val_len).div_ceil(size_of::<u64>())
}

/// What an xattr of type @type_ is listed under, from its handler: Some
/// prefix, or for a POSIX ACL, whose handler has a name instead, None.
/// None at all for a type with no handler: as bch2_xattr_type_to_handler().
fn type_prefix(type_: u8) -> Option<Option<&'static str>> {
    match type_ as u32 {
        c::KEY_TYPE_XATTR_INDEX_USER                => Some(Some("user.")),
        c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS |
        c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT   => Some(None),
        c::KEY_TYPE_XATTR_INDEX_TRUSTED             => Some(Some("trusted.")),
        c::KEY_TYPE_XATTR_INDEX_SECURITY            => Some(Some("security.")),
        _                                           => None,
    }
}

/// @k's type and name - @k an xattr, valid.
fn type_and_name(k: BkeySC<'_>) -> (u8, &[u8]) {
    let x = k.as_xattr().expect("an xattr");
    (x.x_type, &k.val_bytes()[NAME_OFFSET..][..x.x_name_len as usize])
}

/// @k's value - @k an xattr, valid.
pub fn value(k: BkeySC<'_>) -> &[u8] {
    let x = k.as_xattr().expect("an xattr");
    let name_len = x.x_name_len as usize;
    &k.val_bytes()[NAME_OFFSET + name_len..][..u16::from_le(x.x_val_len) as usize]
}

/// Check @v.k, an xattr: as bch2_xattr_validate().
pub fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let x = v.k.as_xattr().expect("an xattr");
    let name_len = x.x_name_len as usize;
    let val_len  = u16::from_le(x.x_val_len) as usize;
    let have     = v.k.val_bytes().len() / size_of::<u64>();

    let want = val_u64s(name_len, val_len);
    bkey_fsck_err_on!(v, have < want, id::xattr_val_size_too_small,
                      "value too small ({} < {})", have, want)?;

    // XXX why +4 ?
    let max = val_u64s(name_len, val_len + 4);
    bkey_fsck_err_on!(v, have > max, id::xattr_val_size_too_big,
                      "value too big ({} > {})", have, max)?;

    bkey_fsck_err_on!(v, type_prefix(x.x_type).is_none(), id::xattr_invalid_type,
                      "invalid type ({})", x.x_type)?;

    let name = &v.k.val_bytes()[NAME_OFFSET..][..name_len];
    bkey_fsck_err_on!(v, name.contains(&0), id::xattr_name_invalid_chars,
                      "xattr name has invalid characters")
}

/// For C's bkey_ops: bch2_xattr_validate().
#[no_mangle]
pub extern "C" fn bch2_xattr_validate(
    c:    &Opaque<c::bch_fs>,
    k:    BkeySC<'_>,
    from: &c::bkey_validate_context,
) -> c_int {
    let fs = Fs::from_c(c);
    let v = BkeyValidate { fs: &fs, k, from };
    ret_to_c(validate(&v))
}

/// The bytes of @b up to its first NUL, as printf's %.*s prints them.
fn to_nul(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

/// @k, an xattr, as text: as bch2_xattr_to_text(). @k may not have passed
/// validate: lengths past the value are cut to it.
pub fn to_text(out: &mut Printbuf, k: BkeySC<'_>) {
    let x = k.as_xattr().expect("an xattr");

    match type_prefix(x.x_type) {
        Some(Some(prefix)) => write!(out, "{prefix}"),
        Some(None)         => write!(out, "(type {})", x.x_type),
        None               => write!(out, "(unknown type {})", x.x_type),
    }

    let bytes    = &k.val_bytes()[NAME_OFFSET..];
    let name_len = (x.x_name_len as usize).min(bytes.len());
    let val_len  = (u16::from_le(x.x_val_len) as usize).min(bytes.len() - name_len);
    let (name, value) = bytes.split_at(name_len);
    let value = &value[..val_len];

    out.write_bytes(to_nul(name));
    write!(out, ":");
    out.write_bytes(to_nul(value));

    if matches!(x.x_type as u32, c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS |
                                 c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT) {
        write!(out, " ");
        acl::to_text(out, value);
    }
}

/// For C's bkey_ops: bch2_xattr_to_text().
#[no_mangle]
#[cold]
pub extern "C" fn bch2_xattr_to_text(
    out: &mut Printbuf,
    _c:  &Opaque<c::bch_fs>,
    k:   BkeySC<'_>,
) {
    to_text(out, k)
}

// ── Get and set ──────────────────────────────────────────────────────────

/// The value of xattr @name of @type_ on inode @inode (@inum), copied into
/// @buffer if there is one: as bch2_xattr_get_trans(). Its length; ERANGE
/// if @buffer is too small, ENOENT if there's no such xattr.
pub fn get_trans(
    t:      &TransAttempt<'_, '_>,
    inode:  &c::bch_inode_unpacked,
    inum:   c::subvol_inum,
    type_:  u8,
    name:   &OsStr,
    buffer: Option<&mut [u8]>,
) -> Result<usize, BchError> {
    let hash_info = str_hash::hash_info_init(t.fs(), inode)?;
    let search = search_key(type_ as u32, name);

    let mut iter = BtreeIter::uninit();
    let k = str_hash::lookup::<Xattrs>(t, &mut iter, &hash_info, inum, &search,
                                       BtreeIterFlags::empty())?;
    copy_out(buffer, value(k))
}

/// @v into @buffer, if there is one - getxattr's: its length, or ERANGE if
/// @buffer is too small.
fn copy_out(buffer: Option<&mut [u8]>, v: &[u8]) -> Result<usize, BchError> {
    if let Some(buffer) = buffer {
        buffer.get_mut(..v.len()).ok_or(BchError::from(c::ERANGE))?.copy_from_slice(v);
    }
    Ok(v.len())
}

/// For C - FUSE's getxattr: get_trans(). The value's length, or an error.
///
/// # Safety
/// @name NUL terminated, @buffer NULL or valid for @size.
#[no_mangle]
pub unsafe extern "C" fn bch2_xattr_get_trans(
    trans:  &Opaque<c::btree_trans>,
    inode:  &c::bch_inode_unpacked,
    inum:   c::subvol_inum,
    type_:  c_int,
    name:   *const c_char,
    buffer: *mut c_void,
    size:   usize,
) -> c_int {
    let buffer = unsafe { opt_bytes_mut(buffer, size) };

    len_to_c(get_trans(&BtreeTrans::from_c(trans).attempt_in_progress(), inode, inum,
                       type_ as u8, unsafe { cstr_name(name) }, buffer))
}

/// A getxattr result as C's int: the length, or -errcode.
fn len_to_c(r: Result<usize, BchError>) -> c_int {
    match r {
        Ok(len) => len as c_int,
        Err(e)  => -e.raw(),
    }
}

/// Set xattr @name of @type_ to @value, on inode @inum - with XATTR_CREATE
/// in @flags only if it doesn't exist, with XATTR_REPLACE only if it does: as
/// __bch2_xattr_set(). ERANGE if it doesn't fit in a key.
pub fn set_key(
    t:         &TransAttempt<'_, '_>,
    inum:      c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
    value:     &[u8],
    type_:     u8,
    flags:     i32,
) -> Result<(), BchError> {
    let name = name.as_bytes();
    let val_u64s = val_u64s(name.len(), value.len());
    if BKEY_U64S + val_u64s > u8::MAX as usize {
        return Err(BchError::from(c::ERANGE));
    }

    let mut k = t.bkey_alloc_init(val_u64s, c::bch_bkey_type::KEY_TYPE_xattr.0 as u8, POS_MIN)?;
    let x = k.k_i_mut().as_mut_xattr().expect("an xattr");
    x.x_type     = type_;
    x.x_name_len = name.len() as u8;
    x.x_val_len  = (value.len() as u16).to_le();

    let mut s = crate::btree::bkey::BkeyS::from(k.k_i_mut());
    let bytes = &mut s.val_bytes_mut()[NAME_OFFSET..];
    bytes[..name.len()].copy_from_slice(name);
    bytes[name.len()..][..value.len()].copy_from_slice(value);

    let mut iter_flags = BtreeIterFlags::empty();
    if flags & XATTR_CREATE != 0 {
        iter_flags |= BtreeIterFlags::STR_HASH_MUST_CREATE;
    }
    if flags & XATTR_REPLACE != 0 {
        iter_flags |= BtreeIterFlags::STR_HASH_MUST_REPLACE;
    }

    str_hash::set::<Xattrs>(t, hash_info, inum, &mut k, iter_flags,
                            UpdateTriggerFlags::empty())
}

/// For C's VFS create, which sets security xattrs: __bch2_xattr_set().
///
/// # Safety
/// @name NUL terminated, @value valid for @size.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn __bch2_xattr_set(
    trans:     &Opaque<c::btree_trans>,
    inum:      c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      *const c_char,
    value:     *const c_void,
    size:      usize,
    type_:     c_int,
    flags:     c_int,
) -> c_int {
    ret_to_c(set_key(&BtreeTrans::from_c(trans).attempt_in_progress(), inum, hash_info,
                     unsafe { cstr_name(name) },
                     unsafe { bytes(value, size) }, type_ as u8, flags))
}

/// Set xattr @name of @type_ on inode @inum to @value, or with None remove
/// it, and update the inode's ctime - @inode is the inode as written: as
/// bch2_xattr_set(). Removing one that isn't there is ENODATA with
/// XATTR_REPLACE, and nothing otherwise.
#[allow(clippy::too_many_arguments)]
pub fn set_trans(
    t:     &TransAttempt<'_, '_>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  &OsStr,
    value: Option<&[u8]>,
    type_: u8,
    flags: i32,
) -> Result<(), BchError> {
    let fs = t.fs();

    subvolume::is_ro_trans(t, inum.subvol as u32)?;

    let mut inode_iter = BtreeIter::uninit();
    *inode = inode::peek(t, &mut inode_iter, inum, BtreeIterFlags::INTENT)?;

    // Besides the ctime update, extents, dirents and xattrs updates require
    // that an inode update also happens - to ensure that if a key exists in
    // one of those btrees with a given snapshot ID an inode is also present
    inode.bi_ctime = fs.current_time();
    inode::write(t, &mut inode_iter, inode)?;

    let hash_info = str_hash::hash_info_init(fs, inode)?;

    let ret = match value {
        Some(value) => set_key(t, inum, &hash_info, name, value, type_, flags),
        None        => str_hash::delete::<Xattrs>(t, &hash_info, inum,
                                                  &search_key(type_ as u32, name)),
    };

    match ret {
        Err(e) if e.matches(c::ENOENT) && flags & XATTR_REPLACE != 0 =>
            Err(BchError::from(c::ENODATA)),
        Err(e) if e.matches(c::ENOENT) => Ok(()),
        r => r,
    }
}

/// set_trans(), for a TransAttempt user: tools copying files in.
#[allow(clippy::too_many_arguments)]
pub fn set(
    t:     &TransAttempt<'_, '_>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  &OsStr,
    val:   &[u8],
    typ:   i32,
    flags: i32,
) -> Result<(), BchError> {
    set_trans(t, inum, inode, name, Some(val), typ as u8, flags)
}

/// For C - FUSE: bch2_xattr_set(). A NULL @value removes it.
///
/// # Safety
/// @name NUL terminated, @value NULL or valid for @size.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn bch2_xattr_set(
    trans: &Opaque<c::btree_trans>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  *const c_char,
    value: *const c_void,
    size:  usize,
    type_: c_int,
    flags: c_int,
) -> c_int {
    let value = unsafe { opt_bytes(value, size) };

    ret_to_c(set_trans(&BtreeTrans::from_c(trans).attempt_in_progress(), inum, inode,
                       unsafe { cstr_name(name) }, value, type_ as u8, flags))
}

#[cfg(kernel)]
pub use vfs::*;

/// The VFS's entry points: listxattr, the handlers xattr.c's tables point
/// at, and the bcachefs.* namespace - kernel only, as C's #ifndef
/// NO_BCACHEFS_FS.
#[cfg(kernel)]
mod vfs {
    use super::*;
    use core::ffi::CStr;
    use crate::btree::bkey::pos;
    use crate::btree::iter::{commit_do, lockrestart_do, CommitFlags};
    use crate::errcode::bch_errcode;
    use crate::opts::{self, InodeOpt, OptChange};
    use crate::util::locking::MemallocFlags;
    use crate::vfs::dentry::Dentry;
    use crate::vfs::inode::{InodeInfo, ATTR_CTIME};

    /// What listxattr writes into: as C's struct xattr_buf. Without a buffer,
    /// only how much would be written is counted.
    struct XattrBuf<'b> {
        buf:  Option<&'b mut [u8]>,
        used: usize,
    }

    impl XattrBuf<'_> {
        /// @prefix and @name, NUL terminated: as __bch2_xattr_emit(). ERANGE
        /// if it doesn't fit.
        fn emit(&mut self, prefix: &[u8], name: &[u8]) -> Result<(), BchError> {
            let total = prefix.len() + name.len() + 1;

            if let Some(buf) = self.buf.as_deref_mut() {
                let out = buf.get_mut(self.used..self.used + total)
                    .ok_or(BchError::from(c::ERANGE))?;
                out[..prefix.len()].copy_from_slice(prefix);
                out[prefix.len()..][..name.len()].copy_from_slice(name);
                out[total - 1] = 0;
            }

            self.used += total;
            Ok(())
        }
    }

    /// @inode's options, as bcachefs.* xattrs - or with @all,
    /// bcachefs_effective.*, inherited ones too: as
    /// bch2_xattr_list_bcachefs().
    fn list_bcachefs(inode: &c::bch_inode_unpacked, buf: &mut XattrBuf<'_>, all: bool)
        -> Result<(), BchError>
    {
        let prefix: &[u8] = if all { b"bcachefs_effective." } else { b"bcachefs." };

        for opt in InodeOpt::all() {
            if opt.get(inode) != 0 && (all || opt.is_own(inode)) {
                buf.emit(prefix, opt.name().to_bytes())?;
            }
        }
        Ok(())
    }

    /// For the VFS: listxattr - bch2_xattr_list().
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[no_mangle]
    pub unsafe extern "C" fn bch2_xattr_list(
        dentry: *mut c::dentry,
        buffer: *mut c_char,
        size:   usize,
    ) -> isize {
        let dentry = unsafe { Dentry::borrow_raw(dentry) };
        let v = dentry.inode();
        let inum = v.inum();
        let mut buf = XattrBuf {
            buf:  unsafe { opt_bytes_mut(buffer as *mut c_void, size) },
            used: 0,
        };

        let trans = crate::btree_trans!(v.fs());
        let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, pos(inum.inum, 0),
                                      BtreeIterFlags::empty());

        let ret = subvolume::for_each_in_subvolume_max_in_trans(
            &trans, &mut iter, pos(inum.inum, u64::MAX), inum.subvol as u32,
            BtreeIterFlags::empty(), |k| {
                if k.k.type_ != c::bch_bkey_type::KEY_TYPE_xattr.0 as u8 {
                    return Ok(());
                }
                let (type_, name) = type_and_name(k);
                match dentry.xattr_list_prefix(type_) {
                    Some(prefix) => buf.emit(prefix.to_bytes(), name),
                    None         => Ok(()),
                }
            })
            .and_then(|_| list_bcachefs(v.inode(), &mut buf, false))
            .and_then(|_| list_bcachefs(v.inode(), &mut buf, true));

        match ret {
            Ok(())  => buf.used as isize,
            Err(e)  => e.class() as isize,
        }
    }

    /// For xattr.c's user, trusted and security handlers: ->get().
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[no_mangle]
    pub unsafe extern "C" fn bch2_xattr_get_handler(
        handler: *const c::xattr_handler,
        _dentry: *mut c::dentry,
        vinode:  *mut c::inode,
        name:    *const c_char,
        buffer:  *mut c_void,
        size:    usize,
    ) -> c_int {
        let v = unsafe { InodeInfo::from_inode(vinode) };
        let type_ = unsafe { c::rust_xattr_handler_flags(handler) } as u8;
        let name = unsafe { cstr_name(name) };
        let mut buffer = unsafe { opt_bytes_mut(buffer, size) };

        let trans = crate::btree_trans!(v.fs());
        let ret = lockrestart_do(&trans, |t| {
            get_trans(t, v.inode(), v.inum(), type_, name, buffer.as_deref_mut())
        });

        match ret {
            Ok(len)                         => len as c_int,
            Err(e) if e.matches(c::ENOENT)  => -(c::ENODATA as c_int),
            Err(e)                          => e.class(),
        }
    }

    /// For xattr.c's user, trusted and security handlers: ->set().
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[no_mangle]
    #[allow(clippy::too_many_arguments)]
    pub unsafe extern "C" fn bch2_xattr_set_handler(
        handler: *const c::xattr_handler,
        _idmap:  *mut c::mnt_idmap,
        _dentry: *mut c::dentry,
        vinode:  *mut c::inode,
        name:    *const c_char,
        value:   *const c_void,
        size:    usize,
        flags:   c_int,
    ) -> c_int {
        let v = unsafe { InodeInfo::from_inode(vinode) };
        let type_ = unsafe { c::rust_xattr_handler_flags(handler) } as u8;
        let name = unsafe { cstr_name(name) };
        let value = unsafe { opt_bytes(value, size) };
        let mut inode = c::bch_inode_unpacked::default();

        let trans = crate::btree_trans!(v.fs());
        let ret = commit_do(&trans, None, CommitFlags::empty(), |t| {
            set_trans(t, v.inum(), &mut inode, name, value, type_, flags)
        });

        match ret {
            Ok(()) => {
                v.update_after_write(&trans, &mut inode, ATTR_CTIME);
                0
            }
            Err(e) => e.class(),
        }
    }

    // bcachefs.* and bcachefs_effective.*: inode options

    /// The value of option @name on @vinode, as text, into @buffer: as
    /// __bch2_xattr_bcachefs_get(). Only an option set on the inode itself
    /// counts, without @all; with it, an inherited one does too.
    fn bcachefs_get(v: &InodeInfo<'_>, name: &CStr, buffer: Option<&mut [u8]>, all: bool)
        -> Result<usize, BchError>
    {
        let fs = v.fs();
        let inode = v.inode();
        let inode_opts = opts::inode_opts_to_opts(inode);

        let Some((id, opt)) = opts::opt_lookup(name)
            .filter(|(id, _)| id.is_inode_opt()) else {
            return Err(fs.err(bch_errcode::BCH_ERR_EINVAL_xattr_get_bad_opt));
        };
        let Some(inode_opt) = id.inode_opt() else {
            return Err(fs.err(bch_errcode::BCH_ERR_EINVAL_xattr_get_not_inode_opt));
        };

        if !opts::opt_defined_by_id(&inode_opts, id) || !(all || inode_opt.is_own(inode)) {
            return Err(BchError::from(c::ENODATA));
        }

        let mut out = Printbuf::new();
        opts::opt_to_text(&mut out, fs, opt, opts::opt_get_by_id(&inode_opts, id));

        if out.as_raw().allocation_failure() {
            return Err(BchError::from(c::ENOMEM));
        }

        copy_out(buffer, out.as_bytes())
    }

    /// For xattr.c's bcachefs.* handler: ->get().
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[no_mangle]
    pub unsafe extern "C" fn bch2_xattr_bcachefs_get(
        _handler: *const c::xattr_handler,
        _dentry:  *mut c::dentry,
        vinode:   *mut c::inode,
        name:     *const c_char,
        buffer:   *mut c_void,
        size:     usize,
    ) -> c_int {
        len_to_c(unsafe {
            bcachefs_get(&InodeInfo::from_inode(vinode), CStr::from_ptr(name),
                         opt_bytes_mut(buffer, size), false)
        })
    }

    /// For xattr.c's bcachefs_effective.* handler: ->get().
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[no_mangle]
    pub unsafe extern "C" fn bch2_xattr_bcachefs_get_effective(
        _handler: *const c::xattr_handler,
        _dentry:  *mut c::dentry,
        vinode:   *mut c::inode,
        name:     *const c_char,
        buffer:   *mut c_void,
        size:     usize,
    ) -> c_int {
        len_to_c(unsafe {
            bcachefs_get(&InodeInfo::from_inode(vinode), CStr::from_ptr(name),
                         opt_bytes_mut(buffer, size), true)
        })
    }

    /// Set inode option @opt on @bi, inode @inum, to @v - biased, 0 for
    /// inheriting - with what changing it needs: casefolding changes the
    /// directory, and 31 bit dirent offsets need an empty one. As
    /// inode_opt_set_fn().
    fn inode_opt_set(
        t:       &TransAttempt<'_, '_>,
        inum:    c::subvol_inum,
        bi:      &mut c::bch_inode_unpacked,
        opt:     InodeOpt,
        v:       u64,
        defined: bool,
    ) -> Result<(), BchError> {
        let fs = t.fs();

        if opt.id() == c::inode_opt_id::Inode_opt_casefold {
            inode::set_casefold(t, inum, bi, v as u32)?;
        }

        if opt.id() == c::inode_opt_id::Inode_opt_inodes_32bit &&
           fs.request_incompat_feature(
               c::bcachefs_metadata_version::bcachefs_metadata_version_31bit_dirent_offset).is_ok() {
            // Make sure the dir is empty, as otherwise we'd need to
            // rehash everything and update the dirent keys.
            crate::dirent::empty_dir_trans(t, inum)?;

            let flag = c::bch_inode_flags::BCH_INODE_31bit_dirent_offset as u32;
            if defined {
                bi.bi_flags |= flag;
            } else {
                bi.bi_flags &= !flag;
            }
        }

        opt.set_own(bi, defined);
        opt.set(bi, v);
        Ok(())
    }

    /// Set option @name on @dentry's inode to @value, or with None, go back
    /// to inheriting it from the parent: as __bch2_xattr_bcachefs_set().
    fn bcachefs_set(dentry: Dentry<'_>, v: &InodeInfo<'_>, name: &CStr, value: Option<&[u8]>)
        -> Result<(), BchError>
    {
        let fs = v.fs();

        let Some((opt_id, opt)) = opts::opt_lookup(name) else {
            return Err(fs.err(bch_errcode::BCH_ERR_EINVAL_xattr_set_bad_opt));
        };
        let Some(inode_opt) = opt_id.inode_opt() else {
            return Err(fs.err(bch_errcode::BCH_ERR_EINVAL_xattr_set_not_inode_opt));
        };

        let defined = value.is_some();
        let mut inode_v = 0;
        let mut v_parsed = 0;

        let _noio = MemallocFlags::noio();
        let mut change = OptChange::new(fs);

        if let Some(value) = value {
            let mut buf = crate::util::alloc::KVVec::new();
            buf.extend_from_slice(value, crate::util::alloc::flags::GFP_KERNEL)
                .map_err(|_| BchError::from(c::ENOMEM))?;
            buf.push(0, crate::util::alloc::flags::GFP_KERNEL)
                .map_err(|_| BchError::from(c::ENOMEM))?;
            let value = CStr::from_bytes_until_nul(&buf).expect("NUL terminated");

            v_parsed = opts::opt_parse(Some(fs), opt, value, None)
                .map_err(|ret| BchError::from_raw(-ret))?;
            change.pre_set(v.inode().bi_inum, opt_id, v_parsed)?;

            // +1 bias for inode options:
            inode_v = v_parsed + 1;
        } else if let Some(parent) = dentry.parent_inode_opt(inode_opt) {
            // Set on the parent: switched back to inheriting from it.
            //
            // rename() also has to deal with keeping inherited options
            // up to date - see bch2_reinherit_attrs()
            inode_v = parent;
        }

        {
            let lock = v.update_lock();

            // inode fields accessible via the xattr interface are stored
            // with a +1 bias, so that 0 means unset:
            if inode_opt.id() == c::inode_opt_id::Inode_opt_project {
                let projid = if inode_v != 0 { inode_v - 1 } else { 0 };
                lock.set_projid(projid as u32)?;
            }

            let inum = v.inum();
            lock.write_inode(0, |t, bi| inode_opt_set(t, inum, bi, inode_opt, inode_v, defined))?;
        }

        if inode_opt.id() == c::inode_opt_id::Inode_opt_casefold {
            dentry.casefold_changed();
        }

        change.post_set(v.inode().bi_inum, opt_id, v_parsed);
        Ok(())
    }

    /// For xattr.c's bcachefs.* handler: ->set().
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[no_mangle]
    #[allow(clippy::too_many_arguments)]
    pub unsafe extern "C" fn bch2_xattr_bcachefs_set(
        _handler: *const c::xattr_handler,
        _idmap:   *mut c::mnt_idmap,
        dentry:   *mut c::dentry,
        vinode:   *mut c::inode,
        name:     *const c_char,
        value:    *const c_void,
        size:     usize,
        _flags:   c_int,
    ) -> c_int {
        let value = unsafe { opt_bytes(value, size) };
        let r = unsafe {
            bcachefs_set(Dentry::borrow_raw(dentry), &InodeInfo::from_inode(vinode),
                         CStr::from_ptr(name), value)
        };
        match r {
            Ok(())  => 0,
            Err(e)  => e.class(),
        }
    }

    /// For xattr.c's bcachefs_effective.* handler: ->set(), which does
    /// nothing - xattrs in that namespace are inherited.
    #[no_mangle]
    #[allow(clippy::too_many_arguments)]
    pub extern "C" fn bch2_xattr_bcachefs_set_effective(
        _handler: *const c::xattr_handler,
        _idmap:   *mut c::mnt_idmap,
        _dentry:  *mut c::dentry,
        _vinode:  *mut c::inode,
        _name:    *const c_char,
        _value:   *const c_void,
        _size:    usize,
        _flags:   c_int,
    ) -> c_int {
        0
    }
}
