// SPDX-License-Identifier: GPL-2.0

//! POSIX ACLs (fs/acl.c): their on-disk form, acl_format.rs's, read and
//! written - and converted to and from the VFS's struct posix_acl, which Rust
//! reaches through the shims of vfs/rust_types.rs.
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
use crate::util::vstructs::{cast_plain, plain_bytes, ArmSized, ArmWalkError};
use crate::util::Printbuf;

pub const ACL_TYPE_ACCESS:  i32 = 0x8000;
pub const ACL_TYPE_DEFAULT: i32 = 0x4000;

impl AclEntry {
    /// An entry as the VFS has it - for ACL_USER and ACL_GROUP, @id the uid
    /// or gid; the others have none: Err(the tag) for a tag ACLs don't have.
    /// Read from a long entry, whose start a short one is, as C overlays them.
    pub fn new(tag: u16, perm: u16, id: u32) -> Result<Self, u16> {
        let e = c::bch_acl_entry { e_tag: tag.into(), e_perm: perm.into(), e_id: id.into() };
        Self::read(plain_bytes(&e)).map_err(|_| tag)
    }

    /// The uid or gid, for ACL_USER and ACL_GROUP; None for the others.
    pub fn id(&self) -> Option<u32> {
        match self {
            Self::user(e) | Self::group(e) => Some(e.e_id.get()),
            _                              => None,
        }
    }
}

/// Why an on-disk ACL isn't one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclError {
    /// Fewer bytes than a header: how many.
    Short(usize),
    /// A bch_acl_header of a version that isn't BCH_ACL_VERSION.
    Version(u32),
    /// An entry that's cut off, or has a tag ACLs don't have - at its byte
    /// offset in the value.
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

/// On-disk ACL @value's entries, past its header: an error if the header
/// isn't one.
fn entry_bytes(value: &[u8]) -> Result<&[u8], AclError> {
    let h = cast_plain::<c::bch_acl_header>(value).ok_or(AclError::Short(value.len()))?;
    if h.a_version.get() != BCH_ACL_VERSION {
        return Err(AclError::Version(h.a_version.get()));
    }
    Ok(&value[HEADER_BYTES..])
}

/// The entries in @entries, an ACL's past its header: one that can't be read
/// is an error, at its offset in the value, which ends the walk.
fn walk(entries: &[u8]) -> impl Iterator<Item = Result<AclEntry, AclError>> + '_ {
    AclEntry::walk(entries)
        .map(|e| e.map_err(|e| AclError::Entry(ArmWalkError { at: HEADER_BYTES + e.at, ..e })))
}

/// The entries of on-disk ACL @value, checked first - the right version,
/// and every entry there, of a tag ACLs have: as bch2_acl_from_disk()'s
/// checks.
pub fn entries(value: &[u8]) -> Result<impl Iterator<Item = AclEntry> + Clone + '_, AclError> {
    let entries = entry_bytes(value)?;
    walk(entries).try_for_each(|e| e.map(|_| ()))?;
    Ok(AclEntry::walk(entries).map(|e| e.expect("an entry checked above")))
}

/// The size @entries take as an on-disk ACL, header included: as
/// bch2_acl_size().
pub fn encoded_len(entries: impl Iterator<Item = AclEntry>) -> usize {
    HEADER_BYTES + entries.map(|e| e.bytes()).sum::<usize>()
}

/// Write @entries into @out as an on-disk ACL - @out encoded_len() of them
/// long.
pub fn encode(entries: impl Iterator<Item = AclEntry>, out: &mut [u8]) {
    let h = c::bch_acl_header { a_version: BCH_ACL_VERSION.into() };
    out[..HEADER_BYTES].copy_from_slice(plain_bytes(&h));

    let mut rest = &mut out[HEADER_BYTES..];
    for e in entries {
        rest = e.write(rest);
    }
    assert!(rest.is_empty(), "encode() given a buffer encoded_len() didn't size");
}

/// On-disk ACL @value, as text: as bch2_acl_to_text(). Its entries, as far
/// as they can be read; where they can't - or the header isn't one - what's
/// wrong, and where.
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

#[cfg(kernel)]
pub use vfs::*;

/// struct posix_acl, in userspace: nothing makes one there - its shims are
/// the VFS's - so there are none, and an Option<&PosixAcl> is always None.
#[cfg(not(kernel))]
pub enum PosixAcl {}

/// struct posix_acl, and the VFS's entry points - kernel only, as C's
/// #ifndef NO_BCACHEFS_FS.
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
    use crate::errcode::{bch_errcode, errptr_to_result, ret_to_c,
                         ret_to_result_void as ret_to_result, BchError};
    use crate::inode;
    use crate::snapshots::subvolume;
    use crate::str_hash;
    use crate::util::ffi::Opaque;
    use crate::util::os_str::{OsStr, OsStrExt};
    use crate::vfs::dentry::Dentry;
    use crate::vfs::inode::{InodeInfo, ATTR_CTIME, ATTR_MODE};
    use crate::xattr::{self, Xattrs};

    /// The xattr type ACLs of @type_ are stored as: as acl_to_xattr_type().
    fn xattr_type(type_: i32) -> u8 {
        match type_ {
            ACL_TYPE_ACCESS  => c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS as u8,
            ACL_TYPE_DEFAULT => c::KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT as u8,
            _ => panic!("acl type {type_}"),
        }
    }

    /// The inode flag saying an ACL of @type_ exists.
    fn inode_flag(type_: i32) -> u32 {
        if type_ == ACL_TYPE_ACCESS {
            c::bch_inode_flags::BCH_INODE_has_access_acl.bits()
        } else {
            c::bch_inode_flags::BCH_INODE_has_default_acl.bits()
        }
    }

    fn search_key(type_: i32) -> xattr::XattrKey<'static> {
        xattr::search_key(xattr_type(type_) as u32, OsStr::from_bytes(b""))
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
            let acl = errptr_to_result(unsafe { c::rust_posix_acl_alloc(t.raw(), nr) })?;
            NonNull::new(acl).map(PosixAcl)
                .ok_or_else(|| t.fs().err(bch_errcode::BCH_ERR_ENOMEM_acl))
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
            unsafe {
                c::rust_posix_acl_set_entry(self.as_raw(), i, e.e_tag(), e.head().e_perm.get(),
                                            e.id().unwrap_or(0))
            }
        }

        /// This ACL with its permissions changed to match file mode @mode:
        /// a new one, as __posix_acl_chmod() makes, unlocking @t to allocate
        /// if it has to.
        fn chmod(self, t: &TransAttempt<'_, '_>, mode: c::umode_t) -> Result<PosixAcl, BchError> {
            let mut acl = self.into_raw();
            let ret = t.result(unsafe { c::rust_posix_acl_chmod(t.raw(), &mut acl, mode) });
            // Ours, whatever happened - so released, on error:
            let acl = NonNull::new(acl).map(PosixAcl);
            ret?;
            Ok(acl.expect("posix_acl_chmod() succeeded without an ACL"))
        }

        /// Cache this - or None, for no ACL - as @v's ACL of @type_.
        fn set_cached(acl: Option<&PosixAcl>, v: &InodeInfo<'_>, type_: i32) {
            let acl = acl.map_or(core::ptr::null_mut(), PosixAcl::as_raw);
            unsafe { c::rust_set_cached_acl(v.vinode(), type_, acl) }
        }
    }

    impl Drop for PosixAcl {
        fn drop(&mut self) {
            unsafe { c::rust_posix_acl_release(self.as_raw()) }
        }
    }

    /// On-disk ACL @value as a posix_acl - None for one with no entries: as
    /// bch2_acl_from_disk(). May unlock to allocate: a restart then.
    fn from_disk(t: &TransAttempt<'_, '_>, value: &[u8]) -> Result<Option<PosixAcl>, BchError> {
        let entries = entries(value).map_err(|err| {
            bch_err!(t.fs(), "invalid acl: {err}");
            BchError::from(c::EINVAL)
        })?;
        let nr = entries.clone().count();
        if nr == 0 {
            return Ok(None);
        }

        let mut acl = PosixAcl::alloc(t, nr as u32)?;
        for (i, e) in entries.enumerate() {
            acl.set_entry(i as u32, e);
        }
        Ok(Some(acl))
    }

    /// @acl as the xattr key storing it, for ACLs of @type_: as
    /// bch2_acl_to_xattr(). E2BIG if it doesn't fit in a key.
    fn to_xattr<'a, 't>(t: &TransAttempt<'a, 't>, acl: &PosixAcl, type_: i32)
        -> Result<TransBkey<'a, 't>, BchError>
    {
        // A tag ACLs don't have: EINVAL, as bch2_acl_to_xattr()
        if acl.entries().any(|e| e.is_err()) {
            return Err(BchError::from(c::EINVAL));
        }
        let entries = || acl.entries().map(|e| e.expect("checked above"));

        let mut k = xattr::new_key(t, xattr_type(type_), b"", encoded_len(entries()))?
            .ok_or(BchError::from(c::E2BIG))?;
        encode(entries(), xattr::value_mut(k.k_i_mut()));
        Ok(k)
    }

    /// For the VFS: ->get_inode_acl(), bch2_get_acl(). NULL for none.
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

        let v = unsafe { InodeInfo::from_inode(vinode) };
        let fs = v.fs();
        let inode = v.inode();
        let inum = v.inum();

        // Fast path: if the inode flag for this ACL type is clear, no acl
        // xattr exists and we can skip the btree lookup. Also populate the
        // VFS negative cache, so subsequent permission checks on this inode
        // short-circuit at the VFS layer:
        if inode.bi_flags & inode_flag(type_) == 0 {
            PosixAcl::set_cached(None, &v, type_);
            return core::ptr::null_mut();
        }

        let hash_info = match str_hash::hash_info_init(fs, inode) {
            Ok(h)  => h,
            Err(e) => return err_ptr(e),
        };
        let search = search_key(type_);

        let trans = crate::btree_trans!(fs);
        let ret = lockrestart_do(&trans, |t| {
            let mut iter = BtreeIter::uninit();
            let k = str_hash::lookup::<Xattrs>(t, &mut iter, &hash_info, inum, &search,
                                               BtreeIterFlags::empty())?;
            from_disk(t, xattr::value(k))
        });

        let acl = match ret {
            Ok(acl)                         => acl,
            Err(e) if e.matches(c::ENOENT)  => None,
            Err(e)                          => return err_ptr(e),
        };

        PosixAcl::set_cached(acl.as_ref(), &v, type_);
        acl.map_or(core::ptr::null_mut(), PosixAcl::into_raw)
    }

    /// Set inode @inum's ACL of @type_ to @acl, or with None remove it, and
    /// its flag saying it has one: as bch2_set_acl_trans(). @inode is the
    /// inode to write; a default ACL is only for directories.
    pub fn set_acl_trans(
        t:     &TransAttempt<'_, '_>,
        inum:  c::subvol_inum,
        inode: &mut c::bch_inode_unpacked,
        acl:   Option<&PosixAcl>,
        type_: i32,
    ) -> Result<(), BchError> {
        let hash_info = str_hash::hash_info_init(t.fs(), inode)?;

        if type_ == ACL_TYPE_DEFAULT && inode.bi_mode as u32 & c::S_IFMT != c::S_IFDIR {
            return if acl.is_none() { Ok(()) } else { Err(BchError::from(c::EACCES)) };
        }

        if let Some(acl) = acl {
            let mut k = to_xattr(t, acl, type_)?;
            str_hash::set::<Xattrs>(t, &hash_info, inum, &mut k, BtreeIterFlags::empty(),
                                    UpdateTriggerFlags::empty())?;
            inode.bi_flags |= inode_flag(type_);
        } else {
            match str_hash::delete::<Xattrs>(t, &hash_info, inum, &search_key(type_)) {
                Err(e) if e.matches(c::ENOENT) => {}
                r => r?,
            }
            inode.bi_flags &= !inode_flag(type_);
        }
        Ok(())
    }

    /// For the VFS: ->set_acl(), bch2_set_acl().
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
            if type_ == ACL_TYPE_ACCESS {
                ret_to_result(unsafe {
                    c::rust_posix_acl_update_mode(idmap, vinode, &mut mode, &mut acl)
                })?;
            }
            // The VFS's reference:
            let acl = unsafe { PosixAcl::borrow_raw(acl) };

            set_acl_trans(t, inum, &mut inode, acl.as_deref(), type_)?;

            inode.bi_ctime = fs.current_time();
            inode.bi_mode  = mode;

            inode::write(t, &mut inode_iter, &mut inode)?;
            t.commit(None, CommitFlags::empty())?;

            v.update_after_write(t.trans(), &mut inode, ATTR_CTIME | ATTR_MODE);
            PosixAcl::set_cached(acl.as_deref(), &v, type_);
            Ok(())
        });

        match ret {
            Ok(())  => 0,
            Err(e)  => e.class(),
        }
    }

    /// Apply new mode @mode to inode @inum's access ACL, if it has one: as
    /// bch2_acl_chmod(). The new ACL, which the caller caches once this has
    /// committed.
    pub fn acl_chmod(
        t:     &TransAttempt<'_, '_>,
        inum:  c::subvol_inum,
        inode: &c::bch_inode_unpacked,
        mode:  c::umode_t,
    ) -> Result<Option<PosixAcl>, BchError> {
        let hash_info = str_hash::hash_info_init(t.fs(), inode)?;
        let search = search_key(ACL_TYPE_ACCESS);

        let mut iter = BtreeIter::uninit();
        let acl = match str_hash::lookup::<Xattrs>(t, &mut iter, &hash_info, inum, &search,
                                                   BtreeIterFlags::INTENT) {
            Ok(k)                           => from_disk(t, xattr::value(k))?,
            Err(e) if e.matches(c::ENOENT)  => return Ok(None),
            Err(e)                          => return Err(e),
        };

        // An ACL with no entries: nothing to change.
        let Some(acl) = acl else { return Ok(None) };

        let acl = acl.chmod(t, mode)?;

        let mut new = to_xattr(t, &acl, ACL_TYPE_ACCESS)?;
        new.k_mut().p = iter.pos();
        t.update(&iter, &new, UpdateTriggerFlags::empty())?;
        Ok(Some(acl))
    }

    /// For C's setattr: bch2_acl_chmod(). @new_acl is left as it was if
    /// there's none.
    #[c_export]
    pub extern "C" fn bch2_acl_chmod(
        trans:   &Opaque<c::btree_trans>,
        inum:    c::subvol_inum,
        inode:   &c::bch_inode_unpacked,
        mode:    c::umode_t,
        new_acl: &mut *mut c::posix_acl,
    ) -> c_int {
        ret_to_c(acl_chmod(&BtreeTrans::from_c(trans).attempt_in_progress(), inum, inode, mode)
                 .map(|acl| if let Some(acl) = acl { *new_acl = acl.into_raw() }))
    }
}
