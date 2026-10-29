// SPDX-License-Identifier: GPL-2.0

//! A mount's or reconfigure's struct fs_context, for a filesystem type T
//! (util/fs_type.rs): its parameters as T parses them, T::Context in
//! fs_private; the log that reaches the caller, errorfc() and friends; and
//! sget, as the two things T can ask of it:
//!
//! - find_super(): the superblock that holds this filesystem, if there is
//!   one. sget_fc() with the test "s_fs_info is this", and a set that refuses
//!   to create.
//! - create_super(): a new one, given the filesystem to own and its block
//!   device. sget_dev(), whose set registers that device - on 7.3, in the
//!   super_dev table that user_get_super() searches, so quotactl() and
//!   ustat() can find it.
//!
//! The filesystem's identity is resolved before either, under the
//! filesystem's own locks. sget_fc() calls test and set under sb_lock, a
//! spinlock, so nothing of T's runs there: test compares a pointer.
//!
//! codegen binds fs_context.h: the kernel crate's fs_context is an opaque
//! declaration. Ours shadows it in c, so the kernel crate's functions that
//! take one, sget_fc() and sget_dev(), get a cast pointer.

use core::ffi::{c_int, CStr};
use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::c;
use crate::util::ffi::Opaque;
use kernel::error::code::{EBUSY, EINVAL, ENOMEM};
use kernel::error::{from_err_ptr, Error, Result};
use kernel::str::CString;

use super::fs_type::{FsType, SuperData};
use super::super_block::{LockedSuper, NewSuper, SuperBlock};

type Data<T> = <<T as FsType>::Data as SuperData>::Target;

#[repr(transparent)]
pub struct FsContext<T: FsType>(Opaque<c::fs_context>, PhantomData<T>);

impl<T: FsType> FsContext<T> {
    /// # Safety
    /// @fc is live for 'a, a T's: its fs_private T::Context (init_fs_context's),
    /// and nobody else's meanwhile, as the VFS calls a context's ops.
    pub(super) unsafe fn from_raw<'a>(fc: *mut c::fs_context) -> &'a mut Self {
        unsafe { &mut *fc.cast::<Self>() }
    }

    pub fn as_ptr(&self) -> *mut c::fs_context {
        self.0.as_ptr()
    }

    fn raw(&self) -> &c::fs_context {
        unsafe { &*self.as_ptr() }
    }

    fn raw_mut(&mut self) -> &mut c::fs_context {
        unsafe { &mut *self.as_ptr() }
    }

    /// T's parameters, as parsed so far.
    pub fn ctx(&mut self) -> &mut T::Context {
        // SAFETY: from_raw()'s
        unsafe { &mut *self.raw().fs_private.cast::<T::Context>() }
    }

    pub fn sb_flags(&self) -> u32 {
        self.raw().sb_flags
    }

    pub fn set_sb_flags(&mut self, flags: u32) {
        self.raw_mut().sb_flags |= flags;
    }

    pub fn rdonly(&self) -> bool {
        self.sb_flags() & c::SB_RDONLY as u32 != 0
    }

    /// The source, as /proc/mounts shows it.
    pub fn source(&self) -> Option<&CStr> {
        let s = self.raw().source;
        (!s.is_null()).then(|| unsafe { CStr::from_ptr(s.cast()) })
    }

    /// Replace the source: the VFS kfree()s whatever's there.
    pub fn set_source(&mut self, s: &CStr) -> Result {
        let new = unsafe { c::kstrdup(s.as_ptr().cast(), c::GFP_KERNEL) };
        if new.is_null() {
            return Err(ENOMEM);
        }

        let fc = self.raw_mut();
        unsafe { c::kfree(fc.source.cast()) };
        fc.source = new;
        Ok(())
    }

    /// For a reconfigure, the superblock being reconfigured: None for a mount.
    pub fn root_super(&self) -> Option<&SuperBlock<Data<T>>> {
        let root = self.raw().root;
        (!root.is_null()).then(|| unsafe { SuperBlock::from_raw((*root).d_sb.cast()) })
    }

    // ── The log ──────────────────────────────────────────────────────────

    /// To the fs_context log, which reaches an fsconfig(2) caller - or dmesg,
    /// for mount(2): logfc(). @prefixed is errorfc()'s, the context's name;
    /// errorf()'s and invalf()'s have none. A message there's no memory to
    /// format is dropped: the error it's about is returned either way.
    fn log(&self, level: u8, prefixed: bool, args: fmt::Arguments<'_>) {
        let Ok(msg) = CString::try_from_fmt(args) else { return };

        let log = &self.raw().log;
        let prefix = if prefixed { log.prefix } else { core::ptr::null() };
        unsafe { c::logfc(log.log, prefix, level as _, c"%s".as_ptr().cast(), msg.as_ptr()) }
    }

    pub fn errorf(&self, args: fmt::Arguments<'_>) {
        self.log(b'e', false, args)
    }

    pub fn errorfc(&self, args: fmt::Arguments<'_>) {
        self.log(b'e', true, args)
    }

    pub fn warnfc(&self, args: fmt::Arguments<'_>) {
        self.log(b'w', true, args)
    }

    /// invalf(): errorf(), and EINVAL to return.
    pub fn invalf(&self, args: fmt::Arguments<'_>) -> T::Error {
        self.errorf(args);
        EINVAL.into()
    }

    // ── sget ─────────────────────────────────────────────────────────────

    /// The superblock holding @key, locked, with an active reference: None when
    /// it has none. @key is only compared with s_fs_info: it may be gone, as an
    /// unpinned pointer to a filesystem may be, and a superblock found is one
    /// pinning it.
    pub fn find_super(&mut self, key: *const Data<T>) -> Result<Option<LockedSuper<T>>> {
        /// sget_fc()'s test: fc->s_fs_info is the key.
        unsafe extern "C" fn is_key(sb: *mut c::super_block, fc: *mut kernel::bindings::fs_context) -> c_int {
            unsafe { ((*sb).s_fs_info == (*fc.cast::<c::fs_context>()).s_fs_info) as c_int }
        }

        /// sget_fc()'s set: a mount that finds no superblock doesn't make one
        /// here. ENOKEY is ours alone to mean that: sget_fc() itself fails with
        /// ENOMEM, EBUSY or EPERM.
        unsafe extern "C" fn no_new(_sb: *mut c::super_block, _fc: *mut kernel::bindings::fs_context) -> c_int {
            -(c::ENOKEY as c_int)
        }

        self.raw_mut().s_fs_info = key.cast_mut().cast();

        // SAFETY: test and set read only what they're passed, under sb_lock
        let sb = unsafe { c::sget_fc(self.as_ptr().cast(), Some(is_key), Some(no_new)) };
        self.raw_mut().s_fs_info = core::ptr::null_mut();

        match from_err_ptr(sb) {
            // SAFETY: a superblock sget_fc() found: locked, active
            Ok(sb) => Ok(Some(unsafe {
                LockedSuper::from_raw(NonNull::new(sb).expect("sget_fc(): a superblock or an error").cast())
            })),
            Err(e) if e == Error::from_errno(-(c::ENOKEY as c_int)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A new superblock owning @data, on block device @bdev - s_bdev, and
    /// s_dev its dev_t, which registers it there, so quotactl() and ustat()
    /// find it by that device. On failure, @data is dropped.
    ///
    /// # Safety
    /// @bdev is live while the superblock is: @data's, say, a device it holds
    /// open until kill_sb drops it.
    pub unsafe fn create_super(&mut self, data: T::Data, bdev: NonNull<c::block_device>)
        -> Result<NewSuper<T>>
    {
        let key = data.into_raw();
        self.raw_mut().s_fs_info = key.as_ptr().cast();

        let sb = unsafe { c::sget_dev(self.as_ptr().cast(), (*bdev.as_ptr()).bd_dev) };

        // sget_fc() moves fc->s_fs_info to a superblock it creates, and leaves
        // it where it was otherwise
        let fc = self.raw_mut();
        let unclaimed = !fc.s_fs_info.is_null();
        fc.s_fs_info = core::ptr::null_mut();

        let reclaim = || drop(unsafe { T::Data::from_raw(key) });

        let sb = match from_err_ptr(sb) {
            Ok(sb) => NonNull::new(sb).expect("sget_dev(): a superblock or an error"),
            Err(e) => {
                reclaim();
                return Err(e);
            }
        };

        if unclaimed {
            // An existing superblock on @dev: whoever created it owns its
            // filesystem, so @data isn't one anything shares.
            unsafe { c::deactivate_locked_super(sb.as_ptr().cast()) };
            reclaim();
            return Err(EBUSY);
        }

        // SAFETY: a superblock sget_fc() created, s_fs_info @data, s_dev
        // @bdev's: s_bdev is that device, which the caller's keeping live
        unsafe {
            (*sb.as_ptr().cast::<c::super_block>()).s_bdev = bdev.as_ptr().cast();
            Ok(NewSuper::from_raw(sb.cast()))
        }
    }
}

// ── A parameter ──────────────────────────────────────────────────────────

/// One parameter of a mount or reconfigure: fsconfig(2)'s, or one of the
/// options a legacy mount(2) string splits into.
#[repr(transparent)]
pub struct FsParam(Opaque<c::fs_parameter>);

/// A parameter's value, by its type - which nothing between fsconfig(2) and
/// parse_param checks against the key: a filesystem that parses by hand has
/// to, or a struct filename or struct file in the union gets read as a
/// string.
pub enum ParamValue<'a> {
    /// No value: a bare flag.
    Flag,
    /// A string, writable: the VFS kfree()s it, so a secret is wiped in place.
    Str(&'a mut [u8]),
    /// A blob, filename or file.
    Other,
}

impl FsParam {
    /// # Safety
    /// @p is live for 'a, and nobody else's meanwhile.
    pub(super) unsafe fn from_raw<'a>(p: *mut c::fs_parameter) -> &'a mut Self {
        unsafe { &mut *p.cast::<Self>() }
    }

    fn raw(&self) -> &c::fs_parameter {
        unsafe { &*self.0.as_ptr() }
    }

    pub fn key(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.raw().key.cast()) }
    }

    /// The key, and the value: separate allocations, so both at once.
    pub fn key_value(&mut self) -> (&CStr, ParamValue<'_>) {
        let p = self.raw();
        // SAFETY: the VFS's, NUL-terminated, apart from the value
        let key = unsafe { CStr::from_ptr(p.key.cast()) };

        let val = match p.type_() {
            c::fs_value_type_fs_value_is_flag => ParamValue::Flag,
            c::fs_value_type_fs_value_is_string => {
                let s = unsafe { p.__bindgen_anon_1.string };
                if s.is_null() {
                    return (key, ParamValue::Flag);
                }
                let len = unsafe { CStr::from_ptr(s.cast()) }.to_bytes().len();
                ParamValue::Str(unsafe { core::slice::from_raw_parts_mut(s.cast(), len) })
            }
            _ => ParamValue::Other,
        };
        (key, val)
    }
}
