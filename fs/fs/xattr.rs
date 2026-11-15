// SPDX-License-Identifier: GPL-2.0

//! Xattrs: extended attributes - a hash table per inode in the xattrs btree,
//! keyed by type and name - and their VFS end: get, set and listxattr, the
//! xattr_handler tables the VFS dispatches through, and the bcachefs.*
//! namespace, which is inode options.
//!
//! POSIX ACLs are xattrs too - types POSIX_ACL_ACCESS and POSIX_ACL_DEFAULT,
//! with an empty name - and are acl.rs's.

use crate::btree::bkey::prelude::*;
pub use crate::cstructs::fs_xattr_format::XattrType;
use core::mem::{offset_of, size_of};

use crate::acl;
use crate::bkey_fsck_err_on;
use crate::btree::bkey::{BKEY_U64S, BkeySC, POS_MIN};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::inode;
use crate::snapshots::subvolume;
use crate::str_hash::{self, HashTable};
use crate::util::os_str::{OsStr, OsStrExt};
use crate::util::vstructs::Validated;
use crate::util::Printbuf;
use crate::format::Btree;

/// setxattr() flags - <uapi/linux/xattr.h>:
pub const XATTR_CREATE:  i32 = 0x1;
pub const XATTR_REPLACE: i32 = 0x2;

// ── The hash table ───────────────────────────────────────────────────────

/// The xattrs btree, as a hash table.
pub struct Xattrs;

/// Keys are a type and a name.
impl HashTable for Xattrs {
    type Key<'k> = XattrKey<'k>;
    type Val = c::Xattr;

    const BTREE: Btree = Btree::xattrs;

    fn hash_key(info: &c::bch_hash_info, key: &XattrKey<'_>) -> u64 {
        hash(info, key.ty, key.name.as_bytes())
    }

    fn hash_bkey(info: &c::bch_hash_info, k: BkeySC<'_, c::Xattr<Validated>>) -> u64 {
        let (ty, name) = k.v().key();
        hash(info, ty, name)
    }

    fn matches(k: BkeySC<'_, c::Xattr<Validated>>, key: &XattrKey<'_>) -> bool {
        k.v().key() == (key.ty, key.name.as_bytes())
    }

    fn same_name(a: BkeySC<'_, c::Xattr<Validated>>, b: BkeySC<'_, c::Xattr<Validated>>) -> bool {
        a.v().key() == b.v().key()
    }
}

/// An xattr's hash under @info: its type, then its name - as
/// bch2_xattr_hash().
fn hash(info: &c::bch_hash_info, ty: XattrType, name: &[u8]) -> u64 {
    str_hash::hash_parts(info, &[&[ty.0 as u8], name], false)
}

/// What an xattr lookup searches for.
pub struct XattrKey<'k> {
    pub ty:   XattrType,
    pub name: &'k OsStr,
}

// ── The value ────────────────────────────────────────────────────────────

/// Where the name starts - the value follows it.
const NAME_OFFSET: usize = offset_of!(c::Xattr, repr.x_name_and_value);

/// The u64s of value an xattr with these takes: as xattr_val_u64s().
pub fn val_u64s(name_len: usize, val_len: usize) -> usize {
    (NAME_OFFSET + name_len + val_len).div_ceil(size_of::<u64>())
}

/// The VFS's names for the types: a prefix, or for a POSIX ACL - whose
/// handler has a name, not a prefix - the whole name.
const TYPE_NAMES: [(&str, XattrType); 5] = [
    ("user.",                    XattrType::USER),
    ("trusted.",                 XattrType::TRUSTED),
    ("security.",                XattrType::SECURITY),
    ("system.posix_acl_access",  XattrType::POSIX_ACL_ACCESS),
    ("system.posix_acl_default", XattrType::POSIX_ACL_DEFAULT),
];

impl XattrType {
    /// Whether this version has a handler for it: bch2_xattr_type_to_handler().
    pub fn known(self) -> bool {
        TYPE_NAMES.iter().any(|&(_, ty)| ty == self)
    }

    pub fn is_acl(self) -> bool {
        matches!(self, Self::POSIX_ACL_ACCESS |
                       Self::POSIX_ACL_DEFAULT)
    }

    /// What its xattrs are listed under - not a POSIX ACL's, which have a name.
    pub fn prefix(self) -> Option<&'static str> {
        TYPE_NAMES.iter().find(|&&(_, ty)| ty == self && !self.is_acl()).map(|&(p, _)| p)
    }

    /// The type of xattr @name and the name it's stored under, past the
    /// prefix: as the VFS's xattr_resolve_name().
    pub fn from_name(name: &[u8]) -> Option<(Self, &[u8])> {
        TYPE_NAMES.iter().find_map(|&(p, ty)| Some((ty, name.strip_prefix(p.as_bytes())?)))
    }
}

/// An xattr's value, as C reads it: valid, its lengths within it.
impl c::Xattr<Validated> {
    pub fn ty(&self) -> XattrType {
        XattrType(self.x_type as u32)
    }

    pub fn name(&self) -> &[u8] {
        &self.x_name_and_value[..self.x_name_len as usize]
    }

    /// What it's hashed and looked up by.
    fn key(&self) -> (XattrType, &[u8]) {
        (self.ty(), self.name())
    }
}

fn xattr(k: BkeySC<'_>) -> &c::Xattr<Validated> {
    k.arm::<c::Xattr>().expect("an xattr").v()
}

/// @k's value - @k an xattr, valid.
pub fn value(k: BkeySC<'_, c::Xattr<Validated>>) -> &[u8] {
    let x = k.v();
    &x.x_name_and_value[x.x_name_len as usize..][..x.x_val_len.get() as usize]
}

/// @k's value, to fill in - @k an xattr, as new_key() makes one.
pub fn value_mut<'k>(k: &'k mut TransBkey<'_, '_, c::Xattr<Validated>>) -> &'k mut [u8] {
    let x = k.v_mut();
    let (name_len, val_len) = (x.x_name_len as usize, x.x_val_len.get() as usize);
    &mut x.x_name_and_value[name_len..][..val_len]
}

/// An xattr key of type @ty named @name, with @val_len bytes of value for
/// the caller to fill in - value_mut(): None if they don't fit in a key.
pub fn new_key<'a, 't>(t: &TransAttempt<'a, 't>, ty: XattrType, name: &[u8], val_len: usize)
    -> Result<Option<TransBkey<'a, 't, c::Xattr<Validated>>>, BchError>
{
    let val_u64s = val_u64s(name.len(), val_len);
    if BKEY_U64S + val_u64s > u8::MAX as usize {
        return Ok(None);
    }

    let mut k = t.bkey_alloc_init::<c::Xattr>(val_u64s, POS_MIN)?;
    let x = k.v_mut();
    x.x_type     = ty.0 as u8;
    x.x_name_len = name.len() as u8;
    x.x_val_len  = (val_len as u16).into();
    x.x_name_and_value[..name.len()].copy_from_slice(name);
    Ok(Some(k))
}

/// The bytes of @b up to its first NUL, as printf's %.*s prints them.
fn to_nul(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

/// @k, an xattr, as text: as bch2_xattr_to_text(). @k may not have passed
/// validate: lengths past the value are cut to it.
pub fn to_text(out: &mut Printbuf, k: BkeySC<'_>) {
    let x = xattr(k);
    let ty = x.ty();

    match ty.prefix() {
        Some(prefix)       => write!(out, "{prefix}"),
        None if ty.known() => write!(out, "(type {})", ty.0),
        None               => write!(out, "(unknown type {})", ty.0),
    }

    let bytes    = &x.x_name_and_value;
    let name_len = (x.x_name_len as usize).min(bytes.len());
    let val_len  = (x.x_val_len.get() as usize).min(bytes.len() - name_len);
    let (name, value) = bytes.split_at(name_len);
    let value = &value[..val_len];

    out.write_bytes(to_nul(name));
    write!(out, ":");
    out.write_bytes(to_nul(value));

    if ty.is_acl() {
        write!(out, " ");
        acl::to_text(out, value);
    }
}

impl crate::btree::bkey::BkeyOps for c::Xattr {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        let x = xattr(v.k);
        let name_len = x.x_name_len as usize;
        let val_len  = x.x_val_len.get() as usize;
        let have     = v.k.val_bytes().len() / size_of::<u64>();

        let want = val_u64s(name_len, val_len);
        bkey_fsck_err_on!(v, have < want, id::xattr_val_size_too_small,
                          "value too small ({} < {})", have, want)?;

        // XXX why +4 ?
        let max = val_u64s(name_len, val_len + 4);
        bkey_fsck_err_on!(v, have > max, id::xattr_val_size_too_big,
                          "value too big ({} > {})", have, max)?;

        bkey_fsck_err_on!(v, !x.ty().known(), id::xattr_invalid_type,
                          "invalid type ({})", x.x_type)?;

        bkey_fsck_err_on!(v, x.name().contains(&0), id::xattr_name_invalid_chars,
                          "xattr name has invalid characters")
    }

    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        to_text(out, k)
    }
}

// ── Get and set ──────────────────────────────────────────────────────────

/// The value of xattr @name of @ty on inode @inode (@inum), copied into
/// @buffer if there is one: as bch2_xattr_get_trans(). Its length; ERANGE
/// if @buffer is too small, ENOENT if there's no such xattr.
pub fn get_trans(
    t:      &TransAttempt<'_, '_>,
    inode:  &c::bch_inode_unpacked,
    inum:   c::subvol_inum,
    ty:     XattrType,
    name:   &OsStr,
    buffer: Option<&mut [u8]>,
) -> Result<usize, BchError> {
    let mut iter = BtreeIter::uninit(t.trans());
    let k = lookup(t, &mut iter, inode, inum, ty, name, BtreeIterFlags::empty())?;
    copy_out(buffer, value(k))
}

/// Xattr @name of @ty on inode @inode (@inum), @iter pointed at it: ENOENT
/// if there's no such xattr.
pub fn lookup<'i, 'a>(
    t:     &'i TransAttempt<'a, '_>,
    iter:  &'i mut BtreeIter<'a>,
    inode: &c::bch_inode_unpacked,
    inum:  c::subvol_inum,
    ty:    XattrType,
    name:  &OsStr,
    flags: BtreeIterFlags,
) -> Result<BkeySC<'i, c::Xattr<Validated>>, BchError> {
    let hash_info = str_hash::hash_info_init(t.fs(), inode)?;
    str_hash::lookup::<Xattrs>(t, iter, &hash_info, inum, &XattrKey { ty, name }, flags)
}

/// @v into @buffer, if there is one - getxattr's: its length, or ERANGE if
/// @buffer is too small.
fn copy_out(buffer: Option<&mut [u8]>, v: &[u8]) -> Result<usize, BchError> {
    if let Some(buffer) = buffer {
        buffer.get_mut(..v.len()).ok_or(BchError::from(c::ERANGE))?.copy_from_slice(v);
    }
    Ok(v.len())
}

/// Set xattr @name of @ty to @value, on inode @inum - with XATTR_CREATE in
/// @flags only if it doesn't exist, with XATTR_REPLACE only if it does: as
/// __bch2_xattr_set(). ERANGE if it doesn't fit in a key.
pub fn set_key(
    t:         &TransAttempt<'_, '_>,
    inum:      c::subvol_inum,
    hash_info: &c::bch_hash_info,
    name:      &OsStr,
    value:     &[u8],
    ty:        XattrType,
    flags:     i32,
) -> Result<(), BchError> {
    let mut k = new_key(t, ty, name.as_bytes(), value.len())?
        .ok_or(BchError::from(c::ERANGE))?;
    value_mut(&mut k).copy_from_slice(value);

    let mut set_flags = str_hash::SetFlags::empty();
    set_flags.set(str_hash::SetFlags::MUST_CREATE,  flags & XATTR_CREATE != 0);
    set_flags.set(str_hash::SetFlags::MUST_REPLACE, flags & XATTR_REPLACE != 0);

    str_hash::set::<Xattrs>(t, hash_info, inum, &mut k, set_flags,
                            UpdateTriggerFlags::empty())
}

/// Set xattr @name of @ty on inode @inum to @value, or with None remove it,
/// and update the inode's ctime - @inode is the inode as written: as
/// bch2_xattr_set(). Removing one that isn't there is ENODATA with
/// XATTR_REPLACE, and nothing otherwise.
#[allow(clippy::too_many_arguments)]
pub fn set_trans(
    t:     &TransAttempt<'_, '_>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    name:  &OsStr,
    value: Option<&[u8]>,
    ty:    XattrType,
    flags: i32,
) -> Result<(), BchError> {
    let fs = t.fs();

    subvolume::is_ro_trans(t, inum.subvol as u32)?;

    let mut inode_iter = BtreeIter::uninit(t.trans());
    *inode = inode::peek(t, &mut inode_iter, inum, BtreeIterFlags::INTENT)?;

    // Besides the ctime update, extents, dirents and xattrs updates require
    // that an inode update also happens - to ensure that if a key exists in
    // one of those btrees with a given snapshot ID an inode is also present
    inode.bi_ctime = fs.current_time();
    inode::write(t, &mut inode_iter, inode)?;

    let hash_info = str_hash::hash_info_init(fs, inode)?;

    let ret = match value {
        Some(value) => set_key(t, inum, &hash_info, name, value, ty, flags),
        None        => str_hash::delete::<Xattrs>(t, &hash_info, inum, &XattrKey { ty, name }),
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
    ty:    XattrType,
    flags: i32,
) -> Result<(), BchError> {
    set_trans(t, inum, inode, name, Some(val), ty, flags)
}

#[cfg(kernel)]
pub use vfs::*;

/// The VFS's entry points: listxattr, the handlers and their tables, and the
/// bcachefs.* namespace - kernel only, as C's #ifndef NO_BCACHEFS_FS.
#[cfg(kernel)]
mod vfs {
    use super::*;
    use crate::format::MetadataVersion;
    use crate::format::BkeyType;
    use core::ffi::{c_int, c_void, CStr};
    use crate::btree::bkey::pos;
    use crate::btree::iter::{lockrestart_do, CommitFlags};
    use crate::errcode::{Errcode, ret_to_class};
    use crate::inode_opts::{self, InodeOpt};
    use crate::locking::MemallocFlags;
    use crate::opts::{self, OptChange};
    use crate::types::CStatic;
    use crate::util::ffi::{c_char, opt_bytes, opt_bytes_mut};
    use cstruct_macros::vtable;
    use crate::vfs::dentry::Dentry;
    use crate::vfs::idmap::RawIdmap;
    use crate::vfs::inode::{InodeInfo, ATTR_CTIME};

    /// A getxattr result as C's int: the length, or the error's class.
    fn len_to_c(r: Result<usize, BchError>) -> c_int {
        match r {
            Ok(len) => len as c_int,
            Err(e)  => e.class(),
        }
    }

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

    /// ->listxattr(): @dentry's xattrs' names, into @buffer - or, without one,
    /// how long they'd be. ERANGE if they don't fit.
    pub fn listxattr(dentry: Dentry<'_>, buffer: Option<&mut [u8]>) -> Result<usize, BchError> {
        let v = dentry.inode();
        let inum = v.inum();
        let mut buf = XattrBuf { buf: buffer, used: 0 };

        let trans = crate::btree_trans!(v.fs());
        let mut iter = BtreeIter::new(&trans, Btree::xattrs, pos(inum.inum, 0),
                                      BtreeIterFlags::empty());

        let ret = subvolume::for_each_in_subvolume_max_in_trans(
            &trans, &mut iter, pos(inum.inum, u64::MAX), inum.subvol as u32,
            BtreeIterFlags::empty(), |_, _, k| {
                if k.k.key_type() != BkeyType::xattr {
                    return Ok(());
                }
                let x = xattr(k);
                match dentry.xattr_list_prefix(x.ty()) {
                    Some(prefix) => buf.emit(prefix.to_bytes(), x.name()),
                    None         => Ok(()),
                }
            })
            .and_then(|_| list_bcachefs(&v.inode(), &mut buf, false))
            .and_then(|_| list_bcachefs(&v.inode(), &mut buf, true));

        ret.map(|()| buf.used)
    }

    /// The value of xattr @name of @ty on @v, into @buffer: the user, trusted
    /// and security handlers' get. ENODATA if there's no such xattr.
    fn stored_get(v: &InodeInfo, ty: XattrType, name: &CStr, mut buffer: Option<&mut [u8]>)
        -> Result<usize, BchError>
    {
        let name = OsStr::from_bytes(name.to_bytes());
        let trans = crate::btree_trans!(v.fs());
        lockrestart_do(&trans, |t| get_trans(t, &v.inode(), v.inum(), ty, name, buffer.as_deref_mut()))
            .map_err(|e| if e.matches(c::ENOENT) { BchError::from(c::ENODATA) } else { e })
    }

    /// Xattr @name of @ty on @v set to @value, or with None removed: the user,
    /// trusted and security handlers' set.
    fn stored_set(v: &InodeInfo, ty: XattrType, name: &CStr, value: Option<&[u8]>, flags: c_int)
        -> Result<(), BchError>
    {
        let name = OsStr::from_bytes(name.to_bytes());
        let mut inode = c::bch_inode_unpacked::default();

        // Updating the VFS inode wants the inode's btree position still
        // locked: in the attempt that wrote it, after its commit.
        let trans = crate::btree_trans!(v.fs());
        lockrestart_do(&trans, |t| {
            set_trans(t, v.inum(), &mut inode, name, value, ty, flags)?;
            t.commit(None, CommitFlags::empty())?;
            v.update_after_write(t.trans(), &inode, ATTR_CTIME);
            Ok(())
        })
    }

    // bcachefs.* and bcachefs_effective.*: inode options

    /// The value of option @name on @vinode, as text, into @buffer: as
    /// __bch2_xattr_bcachefs_get(). Only an option set on the inode itself
    /// counts, without @all; with it, an inherited one does too.
    fn bcachefs_get(v: &InodeInfo, name: &CStr, buffer: Option<&mut [u8]>, all: bool)
        -> Result<usize, BchError>
    {
        let fs = v.fs();
        let inode = &v.inode();
        let inode_opts = inode_opts::inode_opts_to_opts(inode);

        let Some(opt) = name.to_str().ok().and_then(opts::opt_lookup)
            .filter(|opt| opt.id.inode_opt().is_some()) else {
            return Err(fs.err(Errcode::EINVAL_xattr_get_bad_opt));
        };
        let id = opt.id;
        let Some(inode_opt) = id.inode_opt() else {
            return Err(fs.err(Errcode::EINVAL_xattr_get_not_inode_opt));
        };

        if !inode_opts.defined(id) || !(all || inode_opt.is_own(inode)) {
            return Err(BchError::from(c::ENODATA));
        }

        let mut out = Printbuf::new();
        opts::opt_to_text(&mut out, Some(fs), None, opt, inode_opts.get(id), 0);

        if out.allocation_failure() {
            return Err(BchError::from(c::ENOMEM));
        }

        copy_out(buffer, out.as_bytes())
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

        if opt.id() == c::InodeOptId::casefold {
            inode::set_casefold(t, inum, bi, v as u32)?;
        }

        if opt.id() == c::InodeOptId::inodes_32bit &&
           fs.request_incompat_feature(
               MetadataVersion::dirent_offset_31bit).is_ok() {
            // Make sure the dir is empty, as otherwise we'd need to
            // rehash everything and update the dirent keys.
            crate::dirent::empty_dir_trans(t, inum)?;

            bi.set_flag(c::InodeFlags::dirent_offset_31bit, defined);
        }

        opt.set_own(bi, defined);
        opt.set(bi, v);
        Ok(())
    }

    /// Set option @name on @dentry's inode to @value, or with None, go back
    /// to inheriting it from the parent: as __bch2_xattr_bcachefs_set().
    fn bcachefs_set(dentry: Dentry<'_>, v: &InodeInfo, name: &CStr, value: Option<&[u8]>)
        -> Result<(), BchError>
    {
        let fs = v.fs();

        let Some(opt) = name.to_str().ok().and_then(opts::opt_lookup) else {
            return Err(fs.err(Errcode::EINVAL_xattr_set_bad_opt));
        };
        let opt_id = opt.id;
        let Some(inode_opt) = opt_id.inode_opt() else {
            return Err(fs.err(Errcode::EINVAL_xattr_set_not_inode_opt));
        };

        let defined = value.is_some();
        let mut inode_v = 0;
        let mut v_parsed = 0;

        let _noio = MemallocFlags::noio();
        let mut change = OptChange::new(fs);

        if let Some(value) = value {
            // as a C string: up to a NUL
            let value = value.split(|&b| b == 0).next().unwrap_or_default();

            v_parsed = opts::opt_parse(Some(fs), opt, Some(value), None)?;
            change.pre_set(None, v.inode().bi_inum, opt_id, v_parsed)?;

            // +1 bias for inode options:
            inode_v = v_parsed + 1;
        } else if let Some(parent) = dentry.parent_inode_opt(inode_opt) {
            // Set on the parent: switched back to inheriting from it.
            //
            // rename() also has to deal with keeping inherited options
            // up to date - see reinherit_attrs()
            inode_v = parent;
        }

        {
            let lock = v.update_lock();

            // Removing an option the inode doesn't set itself is a no-op -
            // and must not reach inode_opt_set(), which would enable what
            // the option needs
            if !defined && !inode_opt.is_own(&v.inode()) {
                return Err(BchError::from(c::ENODATA));
            }

            // inode fields accessible via the xattr interface are stored
            // with a +1 bias, so that 0 means unset:
            if inode_opt.id() == c::InodeOptId::project {
                let projid = if inode_v != 0 { inode_v - 1 } else { 0 };
                lock.set_projid(projid as u32)?;
            }

            let inum = v.inum();
            lock.write_inode(0, |t, bi| inode_opt_set(t, inum, bi, inode_opt, inode_v, defined))?;
        }

        if inode_opt.id() == c::InodeOptId::casefold {
            dentry.casefold_changed();
        }

        change.post_set(None, v.inode().bi_inum, opt_id, v_parsed);
        Ok(())
    }

    // ── The handlers ─────────────────────────────────────────────────────

    /// The namespace a handler is dispatched for.
    pub(crate) trait XattrPrefix {
        const PREFIX: &'static CStr;
    }

    crate::vfs_ops! {
        /// An xattr namespace's ops: an entry of sb->s_xattr.
        pub(crate) trait XattrHandler: XattrPrefix for c::xattr_handler,
            XattrVTable<T: XattrHandler> = T, |h| { h.prefix = T::PREFIX.as_ptr().cast(); };

        /// Whether @dentry's caller sees the namespace's xattrs listed.
        fn list(dentry: Dentry<'_>) -> bool;
        /// Xattr @name's value into @buffer, or with None, how long it is.
        fn get(inode: &InodeInfo, name: &CStr, buffer: Option<&mut [u8]>) -> Result<usize, BchError>
            => extern (_handler: *const c::xattr_handler, _dentry: *mut c::dentry, inode: *mut c::inode,
                       name: *const c_char, buffer: *mut c_void, size: usize) -> c_int
        {
            len_to_c(unsafe {
                T::get(InodeInfo::from_inode(inode), CStr::from_ptr(name.cast()), opt_bytes_mut(buffer, size))
            })
        };
        /// Xattr @name set to @value - with None, removed - as setxattr(2)'s
        /// @flags say.
        fn set(dentry: Dentry<'_>, inode: &InodeInfo, name: &CStr, value: Option<&[u8]>, flags: c_int)
            -> Result<(), BchError>
            => extern (_handler: *const c::xattr_handler, _idmap: RawIdmap, dentry: *mut c::dentry,
                       inode: *mut c::inode, name: *const c_char, value: *const c_void, size: usize,
                       flags: c_int) -> c_int
        {
            ret_to_class(unsafe {
                T::set(Dentry::borrow_raw(dentry), InodeInfo::from_inode(inode), CStr::from_ptr(name.cast()),
                       opt_bytes(value, size), flags)
            })
        };
    }

    /// The user, trusted and security namespaces: xattrs of type @TY, stored
    /// as they're named.
    struct Stored<const TY: u32>;

    type User     = Stored<{ XattrType::USER.0 }>;
    type Trusted  = Stored<{ XattrType::TRUSTED.0 }>;
    type Security = Stored<{ XattrType::SECURITY.0 }>;

    impl XattrPrefix for User     { const PREFIX: &'static CStr = c"user."; }
    impl XattrPrefix for Trusted  { const PREFIX: &'static CStr = c"trusted."; }
    impl XattrPrefix for Security { const PREFIX: &'static CStr = c"security."; }

    #[vtable]
    impl<const TY: u32> XattrHandler for Stored<TY> where Self: XattrPrefix {
        fn list(_dentry: Dentry<'_>) -> bool {
            TY != XattrType::TRUSTED.0 || crate::os::kernel::capable(crate::os::kernel::Capability::SysAdmin)
        }

        fn get(inode: &InodeInfo, name: &CStr, buffer: Option<&mut [u8]>) -> Result<usize, BchError> {
            stored_get(inode, XattrType(TY), name, buffer)
        }

        fn set(_dentry: Dentry<'_>, inode: &InodeInfo, name: &CStr, value: Option<&[u8]>, flags: c_int)
            -> Result<(), BchError>
        {
            stored_set(inode, XattrType(TY), name, value, flags)
        }
    }

    /// bcachefs.*: the inode's options, those set on it.
    struct Bcachefs;

    impl XattrPrefix for Bcachefs { const PREFIX: &'static CStr = c"bcachefs."; }

    #[vtable]
    impl XattrHandler for Bcachefs {
        fn get(inode: &InodeInfo, name: &CStr, buffer: Option<&mut [u8]>) -> Result<usize, BchError> {
            bcachefs_get(inode, name, buffer, false)
        }

        fn set(dentry: Dentry<'_>, inode: &InodeInfo, name: &CStr, value: Option<&[u8]>, _flags: c_int)
            -> Result<(), BchError>
        {
            bcachefs_set(dentry, inode, name, value)
        }
    }

    /// bcachefs_effective.*: the inode's options, inherited ones too.
    struct BcachefsEffective;

    impl XattrPrefix for BcachefsEffective { const PREFIX: &'static CStr = c"bcachefs_effective."; }

    #[vtable]
    impl XattrHandler for BcachefsEffective {
        fn get(inode: &InodeInfo, name: &CStr, buffer: Option<&mut [u8]>) -> Result<usize, BchError> {
            bcachefs_get(inode, name, buffer, true)
        }

        /// Nothing: xattrs in this namespace are inherited.
        fn set(_dentry: Dentry<'_>, _inode: &InodeInfo, _name: &CStr, _value: Option<&[u8]>, _flags: c_int)
            -> Result<(), BchError>
        {
            Ok(())
        }
    }

    // SAFETY, of the CStatics: immutable, their only parts that aren't Sync
    // pointers to static strings and fns
    static USER_HANDLER: CStatic<c::xattr_handler> = unsafe { CStatic::new(XattrVTable::<User>::OPS) };
    static TRUSTED_HANDLER: CStatic<c::xattr_handler> = unsafe { CStatic::new(XattrVTable::<Trusted>::OPS) };
    static SECURITY_HANDLER: CStatic<c::xattr_handler> = unsafe { CStatic::new(XattrVTable::<Security>::OPS) };
    static BCACHEFS_HANDLER: CStatic<c::xattr_handler> = unsafe { CStatic::new(XattrVTable::<Bcachefs>::OPS) };
    static BCACHEFS_EFFECTIVE_HANDLER: CStatic<c::xattr_handler> = unsafe {
        CStatic::new(XattrVTable::<BcachefsEffective>::OPS)
    };

    /// The VFS's: sb->s_xattr.
    pub static XATTR_HANDLERS: CStatic<[*const c::xattr_handler; 6]> = unsafe {
        CStatic::new([
            core::ptr::addr_of!(USER_HANDLER).cast(),
            core::ptr::addr_of!(TRUSTED_HANDLER).cast(),
            core::ptr::addr_of!(SECURITY_HANDLER).cast(),
            core::ptr::addr_of!(BCACHEFS_HANDLER).cast(),
            core::ptr::addr_of!(BCACHEFS_EFFECTIVE_HANDLER).cast(),
            core::ptr::null(),
        ])
    };

    /// What an xattr of @ty lists under for @dentry's caller - None if it
    /// doesn't: the handler's prefix, or its name for a POSIX ACL's. As
    /// bch2_xattr_list_prefix().
    pub fn list_prefix(ty: XattrType, dentry: Dentry<'_>) -> Option<&'static CStr> {
        match ty {
            XattrType::USER              => User::list(dentry).then_some(User::PREFIX),
            // SAFETY: the kernel's, and immutable
            XattrType::POSIX_ACL_ACCESS  => kernel_list_prefix(unsafe { &c::nop_posix_acl_access }, dentry),
            XattrType::POSIX_ACL_DEFAULT => kernel_list_prefix(unsafe { &c::nop_posix_acl_default }, dentry),
            XattrType::TRUSTED           => Trusted::list(dentry).then_some(Trusted::PREFIX),
            XattrType::SECURITY          => Security::list(dentry).then_some(Security::PREFIX),
            _ => None,
        }
    }

    /// list_prefix() of one of the kernel's handlers: xattr_handler_can_list(),
    /// then xattr_prefix().
    fn kernel_list_prefix(handler: &'static c::xattr_handler, dentry: Dentry<'_>) -> Option<&'static CStr> {
        if let Some(list) = handler.list {
            if !unsafe { list(dentry.as_ptr()) } {
                return None;
            }
        }

        // the handlers' strings are static
        let p = if handler.prefix.is_null() { handler.name } else { handler.prefix };
        Some(unsafe { CStr::from_ptr(p.cast()) })
    }
}
