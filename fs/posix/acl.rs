// SPDX-License-Identifier: GPL-2.0

//! POSIX ACLs: their on-disk form, acl_format.rs's, read and written - and
//! converted to and from the VFS's struct posix_acl.
//!
//! An entry is an AclEntry, a tagged_union! as long as its arm. A tag it
//! doesn't know is one there's no stepping past, so an ACL with one isn't an
//! ACL. An on-disk ACL is checked once, entries(), and read after.
//!
//! An inode records whether it has each kind (BCH_INODE_has_access_acl,
//! _has_default_acl), so that getting the ACL of an inode without one -
//! nearly every inode - needs no lookup.

use core::mem::size_of;

use crate::c::{self, AclEntry, BCH_ACL_VERSION};
use crate::init::error::id;
use crate::os::posix_acl::AclType;
use crate::sb::errors::SbErrorId;
use crate::util::vstructs::{cast_plain, plain_bytes, ArmSized, ArmWalkError};
use crate::util::Printbuf;
use crate::posix::xattr::XattrType;

impl AclEntry {
    /// From the VFS's (tag, perm, id) - @id only for ACL_USER and ACL_GROUP:
    /// Err(the tag) for a tag ACLs don't have. Read as a long entry, whose
    /// start a short one is.
    pub fn new(tag: u16, perm: u16, id: u32) -> Result<Self, u16> {
        let e = c::bch_acl_entry { e_tag: tag.into(), e_perm: perm.into(), e_id: id.into() };
        Self::read(plain_bytes(&e)).map_err(|_| tag)
    }

    /// new()'s (tag, perm, id): the id 0 but for ACL_USER and ACL_GROUP.
    pub fn vfs(&self) -> (u16, u16, u32) {
        let id = match self {
            Self::user(e) | Self::group(e) => e.e_id.get(),
            _                              => 0,
        };
        (self.e_tag(), self.head().e_perm.get(), id)
    }
}

/// Why an on-disk ACL isn't one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclError {
    Short(usize),
    Version(u32),
    /// Cut off, or of a tag ACLs don't have - at its byte offset in the value.
    Entry(ArmWalkError<u16>),
}

impl core::fmt::Display for AclError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short(len)  => write!(f, "{len} bytes, too few for its header"),
            Self::Version(v)  => write!(f, "version {v}, not {BCH_ACL_VERSION}"),
            Self::Entry(err)  => write!(f, "{err}"),
        }
    }
}

const HEADER_BYTES: usize = size_of::<c::bch_acl_header>();

/// Past the header: an error if the header isn't one.
fn entry_bytes(value: &[u8]) -> Result<&[u8], AclError> {
    let h = cast_plain::<c::bch_acl_header>(value).ok_or(AclError::Short(value.len()))?;
    if h.a_version.get() != BCH_ACL_VERSION {
        return Err(AclError::Version(h.a_version.get()));
    }
    Ok(&value[HEADER_BYTES..])
}

/// An entry that can't be read is an error, at its offset in the value, and
/// ends the walk.
fn walk(entries: &[u8]) -> impl Iterator<Item = Result<AclEntry, AclError>> + '_ {
    AclEntry::walk(entries)
        .map(|e| e.map_err(|e| AclError::Entry(ArmWalkError { at: HEADER_BYTES + e.at, ..e })))
}

/// Checked first: the right version, and every entry there of a tag ACLs
/// have.
pub fn entries(value: &[u8]) -> Result<impl Iterator<Item = AclEntry> + Clone + '_, AclError> {
    let entries = entry_bytes(value)?;
    walk(entries).try_for_each(|e| e.map(|_| ()))?;
    Ok(AclEntry::walk(entries).map(|e| e.expect("an entry checked above")))
}

/// Header included.
pub fn encoded_len(entries: impl Iterator<Item = AclEntry>) -> usize {
    HEADER_BYTES + entries.map(|e| e.bytes()).sum::<usize>()
}

/// @out must be encoded_len() long.
pub fn encode(entries: impl Iterator<Item = AclEntry>, out: &mut [u8]) {
    let h = c::bch_acl_header { a_version: BCH_ACL_VERSION.into() };
    out[..HEADER_BYTES].copy_from_slice(plain_bytes(&h));

    let mut rest = &mut out[HEADER_BYTES..];
    for e in entries {
        rest = e.write(rest);
    }
    assert!(rest.is_empty(), "encode() given a buffer encoded_len() didn't size");
}

/// The entries as far as they can be read; where they can't - or the header
/// isn't one - what's wrong, and where.
pub fn to_text(out: &mut Printbuf, value: &[u8]) {
    let mut entries = || -> Result<(), AclError> {
        for (i, e) in walk(entry_bytes(value)?).enumerate() {
            if i > 0 {
                write!(out, " ");
            }
            let e = e?;
            write!(out, "{}", e.name());
            match e {
                AclEntry::user(u)  => write!(out, " uid {}", u.e_id.get()),
                AclEntry::group(g) => write!(out, " gid {}", g.e_id.get()),
                _                  => {}
            }
            write!(out, " {:o}", e.head().e_perm.get());
        }
        Ok(())
    };

    if let Err(err) = entries() {
        write!(out, "(invalid acl: {err})");
    }
}

impl AclType {
    pub fn xattr_type(self) -> XattrType {
        match self {
            Self::Access  => XattrType::POSIX_ACL_ACCESS,
            Self::Default => XattrType::POSIX_ACL_DEFAULT,
        }
    }

    pub fn from_xattr_type(ty: XattrType) -> Option<Self> {
        Self::ALL.into_iter().find(|acl| acl.xattr_type() == ty)
    }

    pub fn inode_flag(self) -> c::InodeFlags {
        match self {
            Self::Access  => c::InodeFlags::has_access_acl,
            Self::Default => c::InodeFlags::has_default_acl,
        }
    }

    /// The fsck error for the inode's flag and its xattrs disagreeing.
    pub fn flag_wrong(self) -> SbErrorId {
        match self {
            Self::Access  => id::inode_has_access_acl_flag_wrong,
            Self::Default => id::inode_has_default_acl_flag_wrong,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Access  => "access",
            Self::Default => "default",
        }
    }
}

#[cfg(kernel)]
pub use vfs::*;

/// struct posix_acl, and the VFS's entry points: kernel only.
#[cfg(kernel)]
mod vfs {
    use super::*;
    use crate::bch_err;
    use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, CommitFlags,
                             TransAttempt, TransBkey, UpdateTriggerFlags};
    use crate::c;
    use crate::errcode::{Errcode, BchError};
    use crate::posix::inode;
    use crate::snapshots::subvolume;
    use crate::posix::str_hash;
    use crate::util::os_str::{OsStr, OsStrExt};
    use crate::util::vstructs::Validated;
    use crate::os::idmap::MntIdmap;
    use crate::os::iattr::{ATTR_CTIME, ATTR_MODE};
    use crate::os::posix_acl::{NewPosixAcl, PosixAcl};
    use crate::vfs::inode::InodeInfo;
    use crate::posix::xattr::{self, Xattrs};

    // ── To and from disk ─────────────────────────────────────────────────

    impl PosixAcl {
        /// None for one with no entries. May unlock to allocate: a restart
        /// then.
        fn from_disk(t: &TransAttempt<'_, '_>, value: &[u8]) -> Result<Option<Self>, BchError> {
            let entries = entries(value).map_err(|err| {
                bch_err!(t.fs(), "invalid acl: {err}");
                t.fs().err(c::Errcode::EINVAL_acl_from_disk)
            })?;
            let nr = entries.clone().count();
            if nr == 0 {
                return Ok(None);
            }

            let mut acl = t.allocate_dropping_locks(|| {
                NewPosixAcl::alloc(nr as u32).ok_or(BchError::from(Errcode::ENOMEM_acl))
            })?;
            for (i, e) in entries.enumerate() {
                acl.set(i, e.vfs());
            }
            Ok(Some(acl.into()))
        }

        /// Inode @inode's (@inum's) ACL of @ty, @iter pointed at its xattr -
        /// None if it has none, or one with no entries.
        fn lookup<'a>(t: &TransAttempt<'a, '_>, iter: &mut BtreeIter<'a>,
                      inode: &c::bch_inode_unpacked, inum: c::subvol_inum, ty: AclType,
                      flags: BtreeIterFlags) -> Result<Option<Self>, BchError> {
            match xattr::lookup(t, iter, inode, inum, ty.xattr_type(), OsStr::from_bytes(b""), flags) {
                Ok(k)                          => Self::from_disk(t, xattr::value(k)),
                Err(e) if e.matches(c::ENOENT) => Ok(None),
                Err(e)                         => Err(e),
            }
        }

        /// The xattr key storing it: E2BIG if it doesn't fit in a key.
        fn to_xattr<'a, 't>(&self, t: &TransAttempt<'a, 't>, ty: AclType)
            -> Result<TransBkey<'a, 't, c::Xattr<Validated>>, BchError>
        {
            let entries = || self.entries().map(|(tag, perm, id)| AclEntry::new(tag, perm, id));

            // A tag ACLs don't have: EINVAL
            if entries().any(|e| e.is_err()) {
                return t.fs().throw(c::Errcode::EINVAL_acl_bad_tag);
            }
            let entries = || entries().map(|e| e.expect("checked above"));

            let mut k = xattr::new_key(t, ty.xattr_type(), b"", encoded_len(entries()))?
                .ok_or(BchError::from(c::E2BIG))?;
            encode(entries(), xattr::value_mut(&mut k));
            Ok(k)
        }
    }

    /// ->get_inode_acl(): @v's ACL of @ty, None for none - and the VFS's cache
    /// of it set.
    pub fn get_acl(v: &InodeInfo, ty: AclType) -> Result<Option<PosixAcl>, BchError> {
        let fs = v.fs();
        let inode = &v.inode();
        let inum = v.inum();

        // The flag clear means there's no ACL xattr: skip the lookup, and have
        // the VFS cache that there's none
        if !inode.flag(ty.inode_flag()) {
            v.vfs().set_cached_acl(ty, None);
            return Ok(None);
        }

        let trans = crate::btree_trans!(fs);
        let acl = lockrestart_do(&trans, |t| {
            let mut iter = BtreeIter::uninit(t.trans());
            PosixAcl::lookup(t, &mut iter, inode, inum, ty, BtreeIterFlags::empty())
        })?;

        v.vfs().set_cached_acl(ty, acl.as_ref());
        Ok(acl)
    }

    /// Set inode @inum's ACL of @ty to @acl, or with None remove it, and its
    /// flag saying it has one. @inode is the inode to write; a default ACL is
    /// only for directories.
    pub fn set_acl_trans(
        t:     &TransAttempt<'_, '_>,
        inum:  c::subvol_inum,
        inode: &mut c::bch_inode_unpacked,
        acl:   Option<&PosixAcl>,
        ty:    AclType,
    ) -> Result<(), BchError> {
        let hash_info = str_hash::hash_info_init(t.fs(), inode)?;

        if ty == AclType::Default && inode.bi_mode as u32 & c::S_IFMT != c::S_IFDIR {
            return if acl.is_none() { Ok(()) } else { t.fs().throw(c::Errcode::EACCES_default_acl_not_dir) };
        }

        if let Some(acl) = acl {
            let k = acl.to_xattr(t, ty)?;
            str_hash::set::<Xattrs>(t, &hash_info, inum, k, str_hash::SetFlags::empty(),
                                    UpdateTriggerFlags::empty())?;
            inode.set_flag(ty.inode_flag(), true);
        } else {
            let search = xattr::XattrKey { ty: ty.xattr_type(), name: OsStr::from_bytes(b"") };
            match str_hash::delete::<Xattrs>(t, &hash_info, inum, &search) {
                Err(e) if e.matches(c::ENOENT) => {}
                r => r?,
            }
            inode.set_flag(ty.inode_flag(), false);
        }
        Ok(())
    }

    /// ->set_acl(): @v's ACL of @ty to @acl, the VFS's - None to remove it - and
    /// the mode with it, through @idmap.
    pub(crate) fn set_acl(idmap: MntIdmap<'_>, v: &InodeInfo, acl: Option<&PosixAcl>, ty: AclType)
        -> Result<(), BchError>
    {
        let fs = v.fs();
        let inum = v.inum();

        let _lock = v.update_lock();

        let trans = crate::btree_trans!(fs);
        lockrestart_do(&trans, |t| {
            let mut acl = acl.map(PosixAcl::borrowed);

            subvolume::is_ro_trans(t, inum.subvol as u32)?;

            let mut inode_iter = BtreeIter::uninit(t.trans());
            let mut inode = inode::peek(t, &mut inode_iter, inum, BtreeIterFlags::INTENT)?;

            // The mode can say all an access ACL would: then there's none.
            let mut mode = inode.bi_mode;
            if ty == AclType::Access {
                idmap.acl_update_mode(v.vfs(), &mut mode, &mut acl)?;
            }

            set_acl_trans(t, inum, &mut inode, acl.as_deref(), ty)?;

            inode.bi_ctime = fs.current_time();
            inode.bi_mode  = mode;

            inode::write(t, &mut inode_iter, &mut inode)?;
            t.commit(None, CommitFlags::empty())?;

            v.update_after_write(t.trans(), &inode, ATTR_CTIME | ATTR_MODE);
            v.vfs().set_cached_acl(ty, acl.as_deref());
            Ok(())
        })
    }

    /// Apply new mode @mode to inode @inum's access ACL, if it has one. The new
    /// ACL, which the caller caches once this has committed.
    pub fn acl_chmod(
        t:     &TransAttempt<'_, '_>,
        inum:  c::subvol_inum,
        inode: &c::bch_inode_unpacked,
        mode:  c::umode_t,
    ) -> Result<Option<PosixAcl>, BchError> {
        // None, or one with no entries: nothing to change.
        let mut iter = BtreeIter::uninit(t.trans());
        let Some(acl) = PosixAcl::lookup(t, &mut iter, inode, inum, AclType::Access,
                                         BtreeIterFlags::INTENT)? else {
            return Ok(None);
        };

        let acl = t.allocate_dropping_locks(|| acl.chmod(mode))?;

        let mut new = acl.to_xattr(t, AclType::Access)?;
        new.k_mut().p = iter.pos();
        t.update(&iter, new, UpdateTriggerFlags::empty())?;
        Ok(Some(acl))
    }
}
