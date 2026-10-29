// SPDX-License-Identifier: GPL-2.0

//! A filesystem type, struct file_system_type, as a trait: FsType. What a
//! mount of it is - get_tree() - and how its parameters parse, over an
//! FsContext (util/fs_context.rs) whose fs_private is the type's Context.
//! Its superblock owns a Data (SuperData), through s_fs_info: given up at
//! create_super(), taken back when kill_sb drops it.
//!
//! FsTypeTable<T> is the struct file_system_type, which register_filesystem()
//! links into the VFS's list, and its ops: the C adapters, here, so T is
//! all Rust.
//!
//! T's errors are its own, T::Error, and T::errno() says what the VFS gets
//! for one. The calls here fail with the kernel's errnos, kernel::error::Error,
//! which T::Error takes.

use core::cell::UnsafeCell;
use core::ffi::{c_int, CStr};
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::c;
use crate::util::alloc::{kalloc_pinned, AllocError, KBox};
use kernel::error::to_result;
use pin_init::Init;

use super::fs_context::{FsContext, FsParam};
use super::super_block::{ReconfSuper, Root, SuperBlock};

/// What a superblock owns, through s_fs_info: one pointer, given up at
/// create_super() and taken back at kill_sb.
///
/// # Safety
/// from_raw(into_raw(x)) is x, and the pointer is to a live Target until then.
pub unsafe trait SuperData: Sized + 'static {
    type Target: 'static;

    fn into_raw(self) -> NonNull<Self::Target>;

    /// # Safety
    /// @p is from into_raw(), and taken back once.
    unsafe fn from_raw(p: NonNull<Self::Target>) -> Self;
}

pub trait FsType: Sized + 'static {
    const NAME: &'static CStr;
    /// fs_flags: FS_REQUIRES_DEV and the rest.
    const FLAGS: c_int;

    type Data: SuperData;
    /// A mount's or reconfigure's parameters, as parsed: fc->fs_private.
    type Context;

    /// What its methods fail with: a kernel errno, from the calls here, or
    /// its own.
    type Error: From<kernel::error::Error> + From<AllocError>;

    /// What the VFS gets for @e: a negative errno, or what this filesystem
    /// returns its callers in place of one.
    fn errno(e: &Self::Error) -> c_int;

    /// A Context before any parameter: an initializer, which init_fs_context
    /// runs in the Context's own allocation - a mount's parameters needn't
    /// fit on the stack.
    fn new_context() -> impl Init<Self::Context, Self::Error>;

    /// Ok's value is returned as is: 0, or what the parameter makes - a file
    /// descriptor, say, which fsconfig(2) returns.
    fn parse_param(fc: &mut FsContext<Self>, param: &mut FsParam) -> Result<c_int, Self::Error>;

    /// The mount: its root, found or made - see FsContext's find_super() and
    /// create_super().
    fn get_tree(fc: &mut FsContext<Self>) -> Result<Root<Self>, Self::Error>;

    fn reconfigure(fc: &mut FsContext<Self>,
                   sb: ReconfSuper<'_, <Self::Data as SuperData>::Target>) -> Result<(), Self::Error>;
}

// ── The table ────────────────────────────────────────────────────────────

/// struct file_system_type for T: a static, registered once at module init.
pub struct FsTypeTable<T: FsType>(UnsafeCell<c::file_system_type>, PhantomData<T>);

// SAFETY: written here only before register(); after, by the VFS, under its
// own locks (file_systems_lock, sb_lock)
unsafe impl<T: FsType> Sync for FsTypeTable<T> {}

impl<T: FsType> FsTypeTable<T> {
    pub const fn new() -> Self {
        // SAFETY: zeroed is a type with no ops, no flags and no supers yet
        let mut t: c::file_system_type = unsafe { core::mem::zeroed() };

        t.name = T::NAME.as_ptr().cast();
        t.fs_flags = T::FLAGS;
        t.init_fs_context = Some(init_fs_context::<T>);
        t.kill_sb = Some(kill_sb::<T>);
        // a module reference while mounted
        #[cfg(kernel)]
        {
            t.owner = &raw mut kernel::bindings::__this_module;
        }

        FsTypeTable(UnsafeCell::new(t), PhantomData)
    }

    pub fn register(&'static self) -> kernel::error::Result {
        to_result(unsafe { c::register_filesystem(self.0.get().cast()) })
    }

    pub fn unregister(&'static self) {
        unsafe { c::unregister_filesystem(self.0.get().cast()) };
    }
}

// ── The adapters ─────────────────────────────────────────────────────────

struct Ops<T>(PhantomData<T>);

impl<T: FsType> Ops<T> {
    const CONTEXT: c::fs_context_operations = c::fs_context_operations {
        free:        Some(free::<T>),
        parse_param: Some(parse_param::<T>),
        get_tree:    Some(get_tree::<T>),
        reconfigure: Some(reconfigure::<T>),
        // SAFETY: zero is NULL, an op not implemented
        ..unsafe { core::mem::zeroed() }
    };
}

/// To C's int: 0, or T::errno().
fn ret<T: FsType>(r: Result<(), T::Error>) -> c_int {
    r.err().map_or(0, |e| T::errno(&e))
}

fn ret_val<T: FsType>(r: Result<c_int, T::Error>) -> c_int {
    r.unwrap_or_else(|e| T::errno(&e))
}

/// file_system_type is the kernel crate's, and so is its fs_context: the same
/// struct as ours, which is whole.
unsafe extern "C" fn init_fs_context<T: FsType>(fc: *mut kernel::bindings::fs_context) -> c_int {
    let fc = fc.cast::<c::fs_context>();
    ret::<T>((|| {
        let ctx = kalloc_pinned(T::new_context())?;

        // SAFETY: a context being set up, ours
        unsafe {
            (*fc).fs_private = ctx.cast();
            (*fc).ops = &Ops::<T>::CONTEXT;
        }
        Ok(())
    })())
}

unsafe extern "C" fn free<T: FsType>(fc: *mut c::fs_context) {
    // SAFETY: init_fs_context()'s - kalloc_pinned()'s is a KBox's - or NULL
    // if it failed
    let ctx = unsafe { (*fc).fs_private.cast::<T::Context>() };
    if !ctx.is_null() {
        drop(unsafe { KBox::from_raw(ctx) });
    }
}

unsafe extern "C" fn parse_param<T: FsType>(fc: *mut c::fs_context, param: *mut c::fs_parameter) -> c_int {
    // SAFETY: the VFS's, for this call
    ret_val::<T>(T::parse_param(unsafe { FsContext::from_raw(fc) }, unsafe { FsParam::from_raw(param) }))
}

unsafe extern "C" fn get_tree<T: FsType>(fc: *mut c::fs_context) -> c_int {
    // SAFETY: the VFS's, for this call
    let fc = unsafe { FsContext::<T>::from_raw(fc) };

    ret::<T>(T::get_tree(fc).map(|root| unsafe { (*fc.as_ptr()).root = root.into_raw().as_ptr().cast() }))
}

unsafe extern "C" fn reconfigure<T: FsType>(fc: *mut c::fs_context) -> c_int {
    // SAFETY: the VFS's, for this call; reconfigure_super() holds the
    // superblock's s_umount for writing, and fc->root is its root
    let fc = unsafe { FsContext::<T>::from_raw(fc) };
    let sb = unsafe { ReconfSuper::new(SuperBlock::from_raw((*(*fc.as_ptr()).root).d_sb.cast())) };

    ret::<T>(T::reconfigure(fc, sb))
}

unsafe extern "C" fn kill_sb<T: FsType>(sb: *mut c::super_block) {
    // SAFETY: the superblock going away, s_umount held for writing; its
    // s_fs_info from create_super(), until here
    unsafe {
        c::generic_shutdown_super(sb);

        if let Some(data) = NonNull::new((*sb).s_fs_info.cast()) {
            (*sb).s_fs_info = core::ptr::null_mut();
            drop(T::Data::from_raw(data));
        }
    }
}
