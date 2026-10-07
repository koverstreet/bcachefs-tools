// SPDX-License-Identifier: GPL-2.0

//! POSIX ACLs (fs/acl.c): stored as xattrs of types POSIX_ACL_ACCESS and
//! POSIX_ACL_DEFAULT with an empty name, the value a bch_acl_header and its
//! entries - and converted to and from the VFS's struct posix_acl, which Rust
//! reaches through vfs/rust.h's shims.
//!
//! An inode records whether it has each kind (BCH_INODE_has_access_acl,
//! _has_default_acl), so that getting the ACL of an inode without one -
//! nearly every inode - needs no lookup.

use crate::util::Printbuf;

// <uapi/linux/posix_acl.h>:
pub const ACL_USER_OBJ:  u16 = 0x01;
pub const ACL_USER:      u16 = 0x02;
pub const ACL_GROUP_OBJ: u16 = 0x04;
pub const ACL_GROUP:     u16 = 0x08;
pub const ACL_MASK:      u16 = 0x10;
pub const ACL_OTHER:     u16 = 0x20;

pub const ACL_TYPE_ACCESS:  i32 = 0x8000;
pub const ACL_TYPE_DEFAULT: i32 = 0x4000;

/// bch_acl_header's a_version:
const BCH_ACL_VERSION: u32 = 0x0001;

const HEADER_SIZE: usize = 4;
/// bch_acl_entry_short: tag and permissions.
const SHORT_SIZE:  usize = 4;
/// bch_acl_entry: and an id, for ACL_USER and ACL_GROUP.
const LONG_SIZE:   usize = 8;

/// One entry of an ACL. @id is the uid or gid, for ACL_USER and ACL_GROUP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AclEntry {
    pub tag:  u16,
    pub perm: u16,
    pub id:   u32,
}

/// How big an entry with @tag is on disk - None for a tag ACLs don't have.
fn entry_size(tag: u16) -> Option<usize> {
    match tag {
        ACL_USER_OBJ | ACL_GROUP_OBJ | ACL_MASK | ACL_OTHER => Some(SHORT_SIZE),
        ACL_USER | ACL_GROUP                                => Some(LONG_SIZE),
        _                                                   => None,
    }
}

fn le16(b: &[u8]) -> u16 { u16::from_le_bytes([b[0], b[1]]) }
fn le32(b: &[u8]) -> u32 { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) }

/// Call @f on each entry of on-disk ACL @value: how many there were, or None
/// if it isn't an ACL - the wrong version, an entry cut off, or a tag ACLs
/// don't have. As bch2_acl_from_disk()'s checks.
pub fn decode(value: &[u8], mut f: impl FnMut(AclEntry)) -> Option<usize> {
    if le32(value.get(..HEADER_SIZE)?) != BCH_ACL_VERSION {
        return None;
    }

    let mut p = &value[HEADER_SIZE..];
    let mut nr = 0;
    while !p.is_empty() {
        let short = p.get(..SHORT_SIZE)?;
        let tag = le16(&short[0..]);
        let size = entry_size(tag)?;
        let e = p.get(..size)?;

        f(AclEntry {
            tag,
            perm: le16(&e[2..]),
            id:   if size == LONG_SIZE { le32(&e[4..]) } else { 0 },
        });
        nr += 1;
        p = &p[size..];
    }
    Some(nr)
}

/// The size @entries take as an on-disk ACL, header included - None if one
/// has a tag ACLs don't have: as bch2_acl_size().
pub fn encoded_len(entries: impl Iterator<Item = AclEntry>) -> Option<usize> {
    let mut len = HEADER_SIZE;
    for e in entries {
        len += entry_size(e.tag)?;
    }
    Some(len)
}

/// Write @entries into @out as an on-disk ACL - @out encoded_len() of them
/// long, so every tag is one ACLs have.
pub fn encode(entries: impl Iterator<Item = AclEntry>, mut out: &mut [u8]) {
    out[..HEADER_SIZE].copy_from_slice(&BCH_ACL_VERSION.to_le_bytes());
    out = &mut out[HEADER_SIZE..];

    for e in entries {
        let size = entry_size(e.tag).expect("a tag ACLs have");
        out[0..2].copy_from_slice(&e.tag.to_le_bytes());
        out[2..4].copy_from_slice(&e.perm.to_le_bytes());
        if size == LONG_SIZE {
            out[4..8].copy_from_slice(&e.id.to_le_bytes());
        }
        out = &mut out[size..];
    }
    assert!(out.is_empty(), "encode() given a buffer encoded_len() didn't size");
}

/// On-disk ACL @value, as text: as bch2_acl_to_text(). Nothing, if it isn't
/// one; an entry with a tag ACLs don't have ends it.
pub fn to_text(out: &mut Printbuf, value: &[u8]) {
    match value.get(..HEADER_SIZE) {
        Some(h) if le32(h) == BCH_ACL_VERSION => {}
        _ => return,
    }

    let mut p = &value[HEADER_SIZE..];
    while let Some(short) = p.get(..SHORT_SIZE) {
        let tag = le16(&short[0..]);

        let name = match tag {
            ACL_USER_OBJ  => "user_obj",
            ACL_USER      => "user",
            ACL_GROUP_OBJ => "group_obj",
            ACL_GROUP     => "group",
            ACL_MASK      => "mask",
            ACL_OTHER     => "other",
            _ => {
                write!(out, "(unknown tag {tag})");
                return;
            }
        };
        write!(out, "{name}");

        let size = entry_size(tag).expect("a known tag");
        let Some(e) = p.get(..size) else { return };
        match tag {
            ACL_USER  => write!(out, " uid {}", le32(&e[4..])),
            ACL_GROUP => write!(out, " gid {}", le32(&e[4..])),
            _         => {}
        }
        write!(out, " {:o}", le16(&e[2..]));

        p = &p[size..];
        if !p.is_empty() {
            write!(out, " ");
        }
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
    use crate::bch_err;
    use crate::btree::bkey::{BKEY_U64S, POS_MIN};
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
            c::bch_inode_flags::BCH_INODE_has_access_acl as u32
        } else {
            c::bch_inode_flags::BCH_INODE_has_default_acl as u32
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

        fn entries(&self) -> impl Iterator<Item = AclEntry> + '_ {
            let nr = unsafe { c::rust_posix_acl_count(self.as_raw()) };
            (0..nr).map(|i| {
                let mut e = AclEntry { tag: 0, perm: 0, id: 0 };
                unsafe {
                    c::rust_posix_acl_entry(self.as_raw(), i, &mut e.tag, &mut e.perm, &mut e.id)
                };
                e
            })
        }

        /// Set entry @i - below the count it was allocated with.
        fn set_entry(&mut self, i: u32, e: AclEntry) {
            assert!(i < unsafe { c::rust_posix_acl_count(self.as_raw()) });
            unsafe { c::rust_posix_acl_set_entry(self.as_raw(), i, e.tag, e.perm, e.id) }
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
        let Some(nr) = decode(value, |_| {}) else {
            bch_err!(t.fs(), "invalid acl entry");
            return Err(BchError::from(c::EINVAL));
        };
        if nr == 0 {
            return Ok(None);
        }

        let mut acl = PosixAcl::alloc(t, nr as u32)?;

        let mut i = 0;
        decode(value, |e| {
            acl.set_entry(i, e);
            i += 1;
        });
        assert_eq!(i as usize, nr);
        Ok(Some(acl))
    }

    /// @acl as the xattr key storing it, for ACLs of @type_: as
    /// bch2_acl_to_xattr(). E2BIG if it doesn't fit in a key.
    fn to_xattr<'a, 't>(t: &TransAttempt<'a, 't>, acl: &PosixAcl, type_: i32)
        -> Result<TransBkey<'a, 't>, BchError>
    {
        let len = encoded_len(acl.entries()).ok_or(BchError::from(c::EINVAL))?;

        let val_u64s = xattr::val_u64s(0, len);
        if BKEY_U64S + val_u64s > u8::MAX as usize {
            return Err(BchError::from(c::E2BIG));
        }

        let mut k = t.bkey_alloc_init(val_u64s, c::bch_bkey_type::KEY_TYPE_xattr.0 as u8,
                                      POS_MIN)?;
        let x = k.k_i_mut().as_mut_xattr().expect("an xattr");
        x.x_type     = xattr_type(type_);
        x.x_name_len = 0;
        x.x_val_len  = (len as u16).to_le();

        let start = core::mem::offset_of!(c::bch_xattr, x_name_and_value);
        let mut s = crate::btree::bkey::BkeyS::from(k.k_i_mut());
        encode(acl.entries(), &mut s.val_bytes_mut()[start..][..len]);

        Ok(k)
    }

    /// For the VFS: ->get_inode_acl(), bch2_get_acl(). NULL for none.
    ///
    /// # Safety
    /// The arguments are the VFS's.
    #[no_mangle]
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
    #[no_mangle]
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
            let mut inode = c::bch_inode_unpacked::default();
            inode::peek(t, &mut inode_iter, &mut inode, inum, BtreeIterFlags::INTENT)?;

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
    #[no_mangle]
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
