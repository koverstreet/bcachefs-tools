// SPDX-License-Identifier: GPL-2.0

//! POSIX ACLs: their on-disk form, acl_format.rs's, read and written - and
//! converted to and from the VFS's struct posix_acl, through the shims of
//! vfs/rust_types.rs.
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
use crate::util::vstructs::{cast_plain, plain_bytes, ArmSized, ArmWalkError};
use crate::util::Printbuf;

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

/// Which of an inode's ACLs: its access ACL, or a directory's default ACL,
/// which what's made in it inherits. The values are the VFS's ACL_TYPE_*.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum AclType {
    Access  = 0x8000,
    Default = 0x4000,
}

impl AclType {
    pub const ALL: [Self; 2] = [Self::Access, Self::Default];

    pub fn xattr_type(self) -> c::bch_xattr_type {
        match self {
            Self::Access  => c::bch_xattr_type::KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS,
            Self::Default => c::bch_xattr_type::KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT,
        }
    }

    pub fn from_xattr_type(ty: c::bch_xattr_type) -> Option<Self> {
        Self::ALL.into_iter().find(|acl| acl.xattr_type() == ty)
    }

    pub fn inode_flag(self) -> c::bch_inode_flags {
        match self {
            Self::Access  => c::bch_inode_flags::BCH_INODE_has_access_acl,
            Self::Default => c::bch_inode_flags::BCH_INODE_has_default_acl,
        }
    }

    /// The fsck error for the inode's flag and its xattrs disagreeing.
    pub fn flag_wrong(self) -> c::bch_sb_error_id {
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

/// struct posix_acl, in userspace: nothing makes one there - its shims are
/// the VFS's - so there are none, and an Option<&PosixAcl> is always None.
#[cfg(not(kernel))]
pub enum PosixAcl {}

/// struct posix_acl, and the VFS's entry points: kernel only.
#[cfg(kernel)]
mod vfs {
    use super::*;
    use core::ffi::c_int;
    use core::mem::ManuallyDrop;
    use core::ptr::NonNull;
    use cstruct_macros::c_export;
    use crate::bch_err;
    use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
                             TransAttempt, TransBkey, UpdateTriggerFlags};
    use crate::c;
    use crate::errcode::{bch_errcode, ret_to_c, ret_to_class, ret_to_result_void as ret_to_result,
                         BchError};
    use crate::inode;
    use crate::snapshots::subvolume;
    use crate::str_hash;
    use crate::util::ffi::Opaque;
    use crate::util::os_str::{OsStr, OsStrExt};
    use crate::vfs::dentry::Dentry;
    use crate::vfs::inode::{InodeInfo, ATTR_CTIME, ATTR_MODE};
    use crate::xattr::{self, Xattrs};

    impl AclType {
        /// Anything else is a bug.
        fn from_vfs(type_: c_int) -> Self {
            match type_ {
                t if t == Self::Access as c_int  => Self::Access,
                t if t == Self::Default as c_int => Self::Default,
                _ => panic!("acl type {type_}"),
            }
        }
    }

    /// ERR_PTR(), for the VFS.
    fn err_ptr<T>(e: BchError) -> *mut T {
        (-(e.raw() as isize)) as *mut T
    }

    /// A struct posix_acl we hold a reference on, released when dropped. One
    /// the caller holds is borrowed - borrow_raw() - and one handed to C, to
    /// keep, goes with into_raw().
    pub struct PosixAcl(NonNull<c::posix_acl>);

    impl PosixAcl {
        /// An ACL of @nr entries, to be filled in: unlocking @t to allocate
        /// if it has to - a restart then.
        fn alloc(t: &TransAttempt<'_, '_>, nr: u32) -> Result<PosixAcl, BchError> {
            let acl = t.allocate_dropping_locks(|gfp| {
                NonNull::new(unsafe { c::posix_acl_alloc(nr, gfp) }).map(PosixAcl)
            })?;
            acl.ok_or_else(|| t.fs().err(bch_errcode::BCH_ERR_ENOMEM_acl))
        }

        /// @acl, which the caller holds a reference on - None for NULL.
        ///
        /// # Safety
        /// @acl is NULL or a live posix_acl, for as long as this is used.
        pub unsafe fn borrow_raw(acl: *mut c::posix_acl) -> Option<ManuallyDrop<PosixAcl>> {
            NonNull::new(acl).map(|acl| ManuallyDrop::new(PosixAcl(acl)))
        }

        /// Our reference, for C to keep.
        pub fn into_raw(self) -> *mut c::posix_acl {
            ManuallyDrop::new(self).0.as_ptr()
        }

        fn as_raw(&self) -> *mut c::posix_acl {
            self.0.as_ptr()
        }

        /// Its entries - Err(the tag) for one with a tag ACLs don't have.
        fn entries(&self) -> impl Iterator<Item = Result<AclEntry, u16>> + '_ {
            let nr = unsafe { c::rust_posix_acl_count(self.as_raw()) };
            (0..nr).map(|i| {
                let (mut tag, mut perm, mut id) = (0, 0, 0);
                unsafe { c::rust_posix_acl_entry(self.as_raw(), i, &mut tag, &mut perm, &mut id) };
                AclEntry::new(tag, perm, id)
            })
        }

        /// Set entry @i - below the count it was allocated with.
        fn set_entry(&mut self, i: u32, e: AclEntry) {
            assert!(i < unsafe { c::rust_posix_acl_count(self.as_raw()) });
            let (tag, perm, id) = e.vfs();
            unsafe { c::rust_posix_acl_set_entry(self.as_raw(), i, tag, perm, id) }
        }

        /// This ACL with its permissions changed to match file mode @mode:
        /// a new one, as __posix_acl_chmod() makes, unlocking @t to allocate
        /// if it has to.
        fn chmod(self, t: &TransAttempt<'_, '_>, mode: c::umode_t) -> Result<PosixAcl, BchError> {
            t.allocate_dropping_locks_errcode(|gfp| {
                // __posix_acl_chmod() consumes the reference it's given -
                // failing too - so each try gets its own:
                let mut acl = self.clone().into_raw();
                let ret = ret_to_result(unsafe { c::__posix_acl_chmod(&mut acl, gfp, mode) });
                let acl = NonNull::new(acl).map(PosixAcl);
                ret?;
                Ok(acl.expect("__posix_acl_chmod() succeeded without an ACL"))
            })
        }

        /// Cache this - or None, for no ACL - as @v's ACL of @ty.
        fn set_cached(acl: Option<&PosixAcl>, v: &InodeInfo<'_>, ty: AclType) {
            let acl = acl.map_or(core::ptr::null_mut(), PosixAcl::as_raw);
            unsafe { c::rust_set_cached_acl(v.vinode(), ty as c_int, acl) }
        }
    }

    /// Another reference: posix_acl_dup().
    impl Clone for PosixAcl {
        fn clone(&self) -> Self {
            PosixAcl(NonNull::new(unsafe { c::rust_posix_acl_dup(self.as_raw()) })
                     .expect("posix_acl_dup() of an ACL"))
        }
    }

    impl Drop for PosixAcl {
        fn drop(&mut self) {
            unsafe { c::rust_posix_acl_release(self.as_raw()) }
        }
    }

    // ── To and from disk ─────────────────────────────────────────────────

    impl PosixAcl {
        /// None for one with no entries. May unlock to allocate: a restart
        /// then.
        fn from_disk(t: &TransAttempt<'_, '_>, value: &[u8]) -> Result<Option<Self>, BchError> {
            let entries = entries(value).map_err(|err| {
                bch_err!(t.fs(), "invalid acl: {err}");
                BchError::from(c::EINVAL)
            })?;
            let nr = entries.clone().count();
            if nr == 0 {
                return Ok(None);
            }

            let mut acl = Self::alloc(t, nr as u32)?;
            for (i, e) in entries.enumerate() {
                acl.set_entry(i as u32, e);
            }
            Ok(Some(acl))
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
            -> Result<TransBkey<'a, 't>, BchError>
        {
            // A tag ACLs don't have: EINVAL
            if self.entries().any(|e| e.is_err()) {
                return Err(BchError::from(c::EINVAL));
            }
            let entries = || self.entries().map(|e| e.expect("checked above"));

            let mut k = xattr::new_key(t, ty.xattr_type(), b"", encoded_len(entries()))?
                .ok_or(BchError::from(c::E2BIG))?;
            encode(entries(), xattr::value_mut(k.k_i_mut()));
            Ok(k)
        }
    }

    /// ->get_inode_acl(). NULL for none.
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[c_export]
    pub unsafe extern "C" fn bch2_get_acl(vinode: *mut c::inode, type_: c_int, rcu: bool)
        -> *mut c::posix_acl
    {
        if rcu {
            return err_ptr(BchError::from(c::ECHILD));
        }

        let ty = AclType::from_vfs(type_);
        let v = unsafe { InodeInfo::from_inode(vinode) };
        let fs = v.fs();
        let inode = v.inode();
        let inum = v.inum();

        // The flag clear means there's no ACL xattr: skip the lookup, and have
        // the VFS cache that there's none
        if inode.bi_flags & ty.inode_flag().bits() == 0 {
            PosixAcl::set_cached(None, &v, ty);
            return core::ptr::null_mut();
        }

        let trans = crate::btree_trans!(fs);
        let acl = match lockrestart_do(&trans, |t| {
            let mut iter = BtreeIter::uninit();
            PosixAcl::lookup(t, &mut iter, inode, inum, ty, BtreeIterFlags::empty())
        }) {
            Ok(acl) => acl,
            Err(e)  => return err_ptr(e),
        };

        PosixAcl::set_cached(acl.as_ref(), &v, ty);
        acl.map_or(core::ptr::null_mut(), PosixAcl::into_raw)
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
            return if acl.is_none() { Ok(()) } else { Err(BchError::from(c::EACCES)) };
        }

        if let Some(acl) = acl {
            let mut k = acl.to_xattr(t, ty)?;
            str_hash::set::<Xattrs>(t, &hash_info, inum, &mut k, BtreeIterFlags::empty(),
                                    UpdateTriggerFlags::empty())?;
            inode.bi_flags |= ty.inode_flag().bits();
        } else {
            let search = xattr::XattrKey { ty: ty.xattr_type(), name: OsStr::from_bytes(b"") };
            match str_hash::delete::<Xattrs>(t, &hash_info, inum, &search) {
                Err(e) if e.matches(c::ENOENT) => {}
                r => r?,
            }
            inode.bi_flags &= !ty.inode_flag().bits();
        }
        Ok(())
    }

    /// ->set_acl().
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[c_export]
    pub unsafe extern "C" fn bch2_set_acl(
        idmap:  *mut c::mnt_idmap,
        dentry: *mut c::dentry,
        acl:    *mut c::posix_acl,
        type_:  c_int,
    ) -> c_int {
        let ty = AclType::from_vfs(type_);
        let v = unsafe { Dentry::borrow_raw(dentry) }.inode();
        let vinode = v.vinode();
        let fs = v.fs();
        let inum = v.inum();

        let _lock = v.update_lock();

        let trans = crate::btree_trans!(fs);
        let ret = lockrestart_do(&trans, |t| {
            let mut acl = acl;

            subvolume::is_ro_trans(t, inum.subvol as u32)?;

            let mut inode_iter = BtreeIter::uninit();
            let mut inode = inode::peek(t, &mut inode_iter, inum, BtreeIterFlags::INTENT)?;

            // The mode can say all an access ACL would: then there's none.
            let mut mode = inode.bi_mode;
            if ty == AclType::Access {
                ret_to_result(unsafe {
                    c::rust_posix_acl_update_mode(idmap, vinode, &mut mode, &mut acl)
                })?;
            }
            // The VFS's reference:
            let acl = unsafe { PosixAcl::borrow_raw(acl) };

            set_acl_trans(t, inum, &mut inode, acl.as_deref(), ty)?;

            inode.bi_ctime = fs.current_time();
            inode.bi_mode  = mode;

            inode::write(t, &mut inode_iter, &mut inode)?;
            t.commit(None, CommitFlags::empty())?;

            v.update_after_write(t, &mut inode, ATTR_CTIME | ATTR_MODE);
            PosixAcl::set_cached(acl.as_deref(), &v, ty);
            Ok(())
        });
        ret_to_class(ret)
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
        let mut iter = BtreeIter::uninit();
        let Some(acl) = PosixAcl::lookup(t, &mut iter, inode, inum, AclType::Access,
                                         BtreeIterFlags::INTENT)? else {
            return Ok(None);
        };

        let acl = acl.chmod(t, mode)?;

        let mut new = acl.to_xattr(t, AclType::Access)?;
        new.k_mut().p = iter.pos();
        t.update(&iter, &new, UpdateTriggerFlags::empty())?;
        Ok(Some(acl))
    }

    /// For C's setattr. @new_acl is left as it was if there's none.
    #[c_export]
    pub extern "C" fn bch2_acl_chmod(
        trans:   &Opaque<c::btree_trans>,
        inum:    c::subvol_inum,
        inode:   &c::bch_inode_unpacked,
        mode:    c::umode_t,
        new_acl: &mut *mut c::posix_acl,
    ) -> c_int {
        ret_to_c(acl_chmod(&unsafe { BtreeTrans::from_c(trans).attempt_in_progress() }, inum, inode, mode)
                 .map(|acl| if let Some(acl) = acl { *new_acl = acl.into_raw() }))
    }
}
