// SPDX-License-Identifier: GPL-2.0

//! debugfs async_objs/: a file per list of in-flight objects. The lists are
//! async_objs.c's, built in the kernel with debugfs, and always in
//! userspace.
//!
//! Everything in flight is to be introspectable, even what's short-lived and
//! high-rate: a fast_list makes listing an object about as cheap as taking a
//! ref. A listable type implements AsyncObj - which list, how it's shown,
//! and, the unsafe part, that it and what it's shown from are freed only a
//! grace period after it's off the list. An object holds its place on the
//! list in a field, an AsyncObjSlot, which takes it off when dropped.
//!
//! XXX: use after free, C's too: the walk is under RCU, but objects are freed
//! at completion - bio_put(), end_io() - with no grace period. Needs
//! async_objs.c in Rust, so fast_list can be typed (its XXX) with an unsafe
//! "freed after a grace period" assertion, as RcuPtr::assign(); then each
//! list's objects RCU-freed, or refcounted against the walker, or removal
//! excluded.

use crate::c;
use crate::fs::Fs;
use crate::util::Printbuf;
use core::ffi::c_void;
#[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
use core::ptr::NonNull;

// ── Listing an object ────────────────────────────────────────────────────

/// # Safety
/// An object on a list is freed only a grace period after it's off it - and
/// so is what its to_text() reads through it: the debugfs walk reads what it
/// finds there under RCU.
pub unsafe trait AsyncObj {
    const LIST: c::bch_async_obj_lists;

    fn to_text(&self, out: &mut Printbuf, fs: &Fs);
}

/// The obj_to_text() for @T's list.
///
/// # Safety
/// @obj is a @T on its list, read under RCU; @out and @c live.
pub unsafe extern "C" fn obj_to_text<T: AsyncObj>(out: *mut c::printbuf, c: *mut c::bch_fs, obj: *mut c_void) {
    // SAFETY: the caller's
    unsafe { (*obj.cast::<T>()).to_text(Printbuf::borrow_raw(out), Fs::borrow_raw(c)) }
}

/// An object's place on its list; empty without the lists configured.
pub struct AsyncObjSlot {
    #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
    list: Option<NonNull<c::fast_list>>,
    #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
    idx:  core::ffi::c_uint,
}

// SAFETY: a list and a slot in it; the list's operations are lockless and
// safe from any context, so any thread may take it off
unsafe impl Send for AsyncObjSlot {}
unsafe impl Sync for AsyncObjSlot {}

impl AsyncObjSlot {
    /// On no list: for an object not listed yet, or that couldn't be.
    pub const fn none() -> Self {
        AsyncObjSlot {
            #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
            list: None,
            #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
            idx:  0,
        }
    }

    /// Puts @obj on its list, if there's room: the lists are a debugging aid,
    /// so no room isn't an error - it's just not listed.
    ///
    /// # Safety
    /// @obj stays where it is, and live, while this is held - it's a field of
    /// it - and @fs outlives this.
    #[allow(unused_variables)]
    pub unsafe fn new<T: AsyncObj>(fs: &Fs, obj: &T) -> Self {
        #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
        {
            let list = &fs.async_objs[T::LIST as usize].list;
            if let Ok(idx) = list.add(NonNull::from(obj).cast()) {
                return AsyncObjSlot { list: Some(NonNull::from(list)), idx };
            }
        }
        Self::none()
    }
}

impl Drop for AsyncObjSlot {
    fn drop(&mut self) {
        #[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
        if let Some(list) = self.list {
            // SAFETY: the filesystem's list, which the caller of new() said
            // outlives us
            unsafe { list.as_ref() }.remove(self.idx);
        }
    }
}

/// Takes an object C listed off its list, for Rust that finishes it. @idx 0
/// means it isn't listed.
///
/// # Safety
/// @idx is the object's, from async_object_list_add() on that list.
#[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
pub unsafe fn list_del(fs: &Fs, list: c::bch_async_obj_lists, idx: &mut core::ffi::c_uint) {
    fs.async_objs[list as usize].list.remove(*idx);
    *idx = 0;
}

// ── The debugfs files ────────────────────────────────────────────────────

#[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
pub(crate) use files::debugfs_init;

#[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
mod files {
use core::ffi::CStr;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::debugfs::{self, DebugFile, DebugOut};
use crate::util::ffi::Opaque;
use crate::util::place::Placed;
use crate::util::rcu;

impl c::async_obj_list {
    /// The filesystem it's one of the async_objs[] of.
    fn fs(&self) -> &Fs {
        self.container()
    }
}

/// The cursor is the next slot.
#[derive(Default)]
struct AsyncObjList {
    next: usize,
}

impl DebugFile for AsyncObjList {
    type Obj = c::async_obj_list;

    // bch2_async_obj_list_open() didn't set may_vmalloc
    const MAY_VMALLOC: bool = false;

    fn read(&mut self, list: &c::async_obj_list, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        let fs = list.fs();
        let to_text = list.obj_to_text.expect("async_obj_list without its obj_to_text");

        out.print_each(|out| {
            let rcu = rcu::read_lock();
            let Some((idx, obj)) = list.list.items_from(self.next, &rcu).next() else {
                return Ok(false);
            };

            // atomic: under RCU, the printbuf mustn't sleep to grow. SAFETY:
            // an object of the list's kind - but see the XXX
            unsafe { to_text(out.atomic().as_raw(), core::ptr::from_ref(fs).cast_mut(), obj.as_ptr()) };
            self.next = idx + 1;
            Ok(true)
        })
    }
}

/// Indexed as async_objs[].
macro_rules! list_names {
    ($(($name:ident)),* $(,)?) => {
        const LIST_NAMES: [&CStr; c::BCH_ASYNC_OBJ_NR as usize] = [$(
            match CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()) {
                Ok(name) => name,
                Err(_)   => panic!(),
            },
        )*];
    };
}
c::BCH_ASYNC_OBJ_LISTS!(list_names);

/// bch2_fs_debug_init()'s, in its directory @parent.
pub(crate) fn debugfs_init(c: &Opaque<c::bch_fs>, parent: NonNull<c::dentry>) {
    let dir = debugfs::create_dir(c"async_objs", Some(parent));
    // SAFETY: filesystem init, before anything reads it
    unsafe { (*c.as_ptr()).async_obj_dir = dir.map_or(core::ptr::null_mut(), NonNull::as_ptr) };

    let Some(dir) = dir else { return };
    let fs = Fs::from_c(c);
    for (list, name) in fs.async_objs.iter().zip(LIST_NAMES) {
        // SAFETY: bch2_fs_debug_exit() removes them first
        unsafe { debugfs::create_file::<AsyncObjList>(name, dir, list) };
    }
}
}
