// SPDX-License-Identifier: GPL-2.0

//! The async object lists - c->async_objs[] - and debugfs async_objs/, a
//! file per list.
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
//! publishing a NonNull into an RcuPtr.

#![allow(non_camel_case_types)]

use crate::c;
use crate::fs::Fs;
use crate::util::Printbuf;
use core::ffi::c_uint;
use cstruct_macros::{c_enum, placed, CStruct, Init};
use pin_init::pin_data;

c_enum! {
    #[closed]
    #[prefix(BCH_ASYNC_OBJ_LIST_)]
    #[nr(BCH_ASYNC_OBJ_NR)]
    pub enum bch_async_obj_lists: u32 {
        data_update,
        rbio,
        write_op,
        btree_read_bio,
        btree_write_bio,
    }
}

#[placed(Fs.async_objs[], index = idx: c_uint)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct async_obj_list {
    pub list: c::fast_list,
}

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

/// An object's place on its list.
pub struct AsyncObjSlot {
    list: Option<NonNull<c::fast_list>>,
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
            list: None,
            idx:  0,
        }
    }

    /// Puts @obj on its list, if there's room: the lists are a debugging aid,
    /// so no room isn't an error - it's just not listed.
    ///
    /// # Safety
    /// @obj stays where it is, and live, while this is held - it's a field of
    /// it - and @fs outlives this.
    pub unsafe fn new<T: AsyncObj>(fs: &Fs, obj: &T) -> Self {
        let list = &fs.async_objs[T::LIST as usize].list;
        match list.add(NonNull::from(obj).cast()) {
            Ok(idx) => AsyncObjSlot { list: Some(NonNull::from(list)), idx },
            Err(_)  => Self::none(),
        }
    }
}

impl Drop for AsyncObjSlot {
    fn drop(&mut self) {
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
pub unsafe fn list_del(fs: &Fs, list: c::bch_async_obj_lists, idx: &mut core::ffi::c_uint) {
    fs.async_objs[list as usize].list.remove(*idx);
    *idx = 0;
}

// ── The lists, and their debugfs files ───────────────────────────────────

pub(crate) use lists::debugfs_init;

pub mod lists {
use core::ffi::{c_void, CStr};
use core::ptr::NonNull;

use super::AsyncObj;
use crate::c;
use crate::data::update::bch2_read_bio_to_text;
use crate::data::update_task::Update;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::bio::Bio;
use crate::util::debugfs::{self, DebugFile, DebugOut};
use crate::util::place::Contained;
use crate::util::rcu;
use crate::util::{Printbuf, ToText};

pub fn async_obj_init(fs: &Fs) -> Result<(), BchError> {
    for l in &fs.async_objs {
        // SAFETY: filesystem init, before anything is listed
        if unsafe { l.list.init() }.is_err() {
            return fs.throw(c::bch_errcode::BCH_ERR_ENOMEM_async_obj_init);
        }
    }
    Ok(())
}

pub fn async_obj_exit(fs: &Fs) {
    for l in &fs.async_objs {
        // SAFETY: filesystem teardown, everything done with
        unsafe { l.list.exit() };
    }
}

cstruct_macros::c_extern! {
    fn bch2_btree_read_bio_to_text(out: *mut Printbuf, rbio: *mut c::btree_read_bio);
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
    crate::data::write::write_op_to_text(out, unsafe { obj.cast::<c::bch_write_op>().as_ref() })
}

#[cold]
unsafe fn btree_read_bio_obj_to_text(out: &mut Printbuf, _: &Fs, obj: NonNull<c_void>) {
    unsafe { bch2_btree_read_bio_to_text(out.as_raw(), obj.as_ptr().cast()) }
}

#[cold]
unsafe fn btree_write_bio_obj_to_text(out: &mut Printbuf, _: &Fs, obj: NonNull<c_void>) {
    let wbio = obj.cast::<crate::btree::write::BtreeWriteBio>().as_ptr();
    unsafe { Bio::from_raw(&raw const (*wbio).wbio.bio) }.to_text(out)
}

macro_rules! list_table {
    ($(($name:ident, $nr:expr)),* $(,)?) => { crate::paste! {
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
bch_async_obj_lists_arms!(list_table);

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

            // SAFETY: an object of the list's kind - but see the XXX
            unsafe { to_text(out, fs, obj) };
            self.next = idx + 1;
            Ok(true)
        })
    }
}

/// debug_init()'s, in its directory @parent.
pub(crate) fn debugfs_init(fs: &Fs, parent: NonNull<c::dentry>) {
    let dir = debugfs::create_dir(c"async_objs", Some(parent));
    // SAFETY: filesystem init, before anything reads it
    unsafe { (*fs.as_ptr()).async_obj_dir = dir.map_or(core::ptr::null_mut(), NonNull::as_ptr) };

    let Some(dir) = dir else { return };
    for (list, (name, _)) in fs.async_objs.iter().zip(LISTS) {
        // SAFETY: bch2_fs_debug_exit() removes them first
        unsafe { debugfs::create_file::<AsyncObjList>(name, dir, list) };
    }
}
}
