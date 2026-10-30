// SPDX-License-Identifier: GPL-2.0

//! The async object lists - c->async_objs[], built in the kernel with
//! CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS, and always in userspace - and debugfs
//! async_objs/, a file per list.
//!
//! Everything in flight is to be introspectable, even what's short-lived and
//! high-rate: a fast_list makes listing an object about as cheap as taking a
//! ref. A listable type implements AsyncObj - which list, how it's shown,
//! and, the unsafe part, that it and what it's shown from are freed only a
//! grace period after it's off the list. An object holds its place on the
//! list in a field, an AsyncObjSlot, which takes it off when dropped. C's
//! objects are listed by async_objs.h's macros, and shown by their C
//! to_text()s, through LISTS.
//!
//! XXX: use after free, C's too: the walk is under RCU, but objects are freed
//! at completion - bio_put(), end_io() - with no grace period. Each list's
//! objects RCU-freed is a series of its own; then fast_list can be typed (its
//! XXX), with an unsafe "freed after a grace period" assertion, as
//! RcuPtr::assign().

use crate::c;
use crate::fs::Fs;
use crate::util::Printbuf;
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

// ── The lists, and their debugfs files ───────────────────────────────────

#[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
pub(crate) use lists::debugfs_init;

#[cfg(CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS)]
pub mod lists {
use core::ffi::{c_int, c_void, CStr};
use core::ptr::NonNull;
use cstruct_macros::c_export;

use super::AsyncObj;
use crate::c;
use crate::data::update::bch2_read_bio_to_text;
use crate::data::update_task::Update;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::util::debugfs::{self, DebugFile, DebugOut};
use crate::util::ffi::Opaque;
use crate::util::place::Placed;
use crate::util::rcu;
use crate::util::Printbuf;

#[c_export]
pub fn bch2_fs_async_obj_init(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    for l in &fs.async_objs {
        // SAFETY: filesystem init, before anything is listed
        if unsafe { l.list.init() }.is_err() {
            return ret_to_c(Err(fs.err(c::bch_errcode::BCH_ERR_ENOMEM_async_obj_init)));
        }
    }
    0
}

#[c_export]
pub fn bch2_fs_async_obj_exit(c: &Opaque<c::bch_fs>) {
    for l in &Fs::from_c(c).async_objs {
        // SAFETY: filesystem teardown, everything done with
        unsafe { l.list.exit() };
    }
}

cstruct_macros::c_extern! {
    fn bch2_write_op_to_text(out: *mut c::printbuf, op: *mut c::bch_write_op);
    fn bch2_btree_read_bio_to_text(out: *mut c::printbuf, rbio: *mut c::btree_read_bio);
    fn bch2_bio_to_text(out: *mut c::printbuf, bio: *mut c::bio);
}

/// How a list's objects are shown: of the list's kind, read under RCU.
type ObjToText = unsafe fn(&mut Printbuf, &Fs, NonNull<c_void>);

// Each list's, named for it. SAFETY, for each: the caller's - but see the XXX

#[cold]
unsafe fn data_update_obj_to_text(out: &mut Printbuf, fs: &Fs, obj: NonNull<c_void>) {
    unsafe { obj.cast::<Update>().as_ref() }.to_text(out, fs)
}

#[cold]
unsafe fn rbio_obj_to_text(out: &mut Printbuf, fs: &Fs, obj: NonNull<c_void>) {
    unsafe { bch2_read_bio_to_text(out.as_raw(), fs.as_ptr(), obj.as_ptr().cast()) }
}

#[cold]
unsafe fn write_op_obj_to_text(out: &mut Printbuf, _: &Fs, obj: NonNull<c_void>) {
    unsafe { bch2_write_op_to_text(out.as_raw(), obj.as_ptr().cast()) }
}

#[cold]
unsafe fn btree_read_bio_obj_to_text(out: &mut Printbuf, _: &Fs, obj: NonNull<c_void>) {
    unsafe { bch2_btree_read_bio_to_text(out.as_raw(), obj.as_ptr().cast()) }
}

#[cold]
unsafe fn btree_write_bio_obj_to_text(out: &mut Printbuf, _: &Fs, obj: NonNull<c_void>) {
    let wbio = obj.cast::<c::btree_write_bio>().as_ptr();
    unsafe { bch2_bio_to_text(out.as_raw(), &raw mut (*wbio).wbio.bio) }
}

macro_rules! list_table {
    ($(($name:ident)),* $(,)?) => { crate::paste! {
        /// Each list's name, and how its objects are shown: indexed as
        /// async_objs[].
        const LISTS: [(&CStr, ObjToText); c::BCH_ASYNC_OBJ_NR as usize] = [$((
            match CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()) {
                Ok(name) => name,
                Err(_)   => panic!(),
            },
            [<$name _obj_to_text>],
        )),*];
    }};
}
c::BCH_ASYNC_OBJ_LISTS!(list_table);

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
        let (_, to_text) = LISTS[list.idx.get() as usize];

        out.print_each(|out| {
            let rcu = rcu::read_lock();
            let Some((idx, obj)) = list.list.items_from(self.next, &rcu).next() else {
                return Ok(false);
            };

            // atomic: under RCU, the printbuf mustn't sleep to grow. SAFETY:
            // an object of the list's kind - but see the XXX
            unsafe { to_text(&mut out.atomic(), fs, obj) };
            self.next = idx + 1;
            Ok(true)
        })
    }
}

/// bch2_fs_debug_init()'s, in its directory @parent.
pub(crate) fn debugfs_init(c: &Opaque<c::bch_fs>, parent: NonNull<c::dentry>) {
    let dir = debugfs::create_dir(c"async_objs", Some(parent));
    // SAFETY: filesystem init, before anything reads it
    unsafe { (*c.as_ptr()).async_obj_dir = dir.map_or(core::ptr::null_mut(), NonNull::as_ptr) };

    let Some(dir) = dir else { return };
    let fs = Fs::from_c(c);
    for (list, (name, _)) in fs.async_objs.iter().zip(LISTS) {
        // SAFETY: bch2_fs_debug_exit() removes them first
        unsafe { debugfs::create_file::<AsyncObjList>(name, dir, list) };
    }
}
}
