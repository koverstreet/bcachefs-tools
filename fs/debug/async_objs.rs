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

use core::ffi::c_void;
use core::ptr::NonNull;
use crate::data_structures::fast_list::FastList;
use crate::errcode::Errcode;
use crate::fs::Fs;
use crate::util::{alloc::init_or, task::TaskList, Printbuf};
use cstruct_macros::{bch_enum, placed, Init, MaybeZeroable};
use pin_init::pin_data;

#[bch_enum(closed)]
#[nr]
#[repr(u32)]
pub enum AsyncObjKind {
    data_update,
    rbio,
    task,
}

#[placed(Fs.async_objs[], index = idx: u32)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct AsyncObjList {
    #[pin]
    #[init(init_or(FastList::new(), Errcode::ENOMEM_async_obj_init))]
    pub list: FastList<c_void>,
}

// ── Listing an object ────────────────────────────────────────────────────

/// # Safety
/// An object on a list is freed only a grace period after it's off it - and
/// so is what its to_text() reads through it: the debugfs walk reads what it
/// finds there under RCU.
pub unsafe trait AsyncObj {
    const LIST: AsyncObjKind;

    fn to_text(&self, out: &mut Printbuf, fs: &Fs);
}

/// An object's place on its list.
pub struct AsyncObjSlot {
    list: Option<NonNull<FastList<c_void>>>,
    idx:  u32,
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
pub unsafe fn list_del(fs: &Fs, list: AsyncObjKind, idx: &mut u32) {
    fs.async_objs[list as usize].list.remove(*idx);
    *idx = 0;
}

impl Fs {
    pub fn tasks(&self) -> &TaskList {
        TaskList::of(&self.async_objs[AsyncObjKind::task as usize].list)
    }
}

// ── The lists, and their debugfs files ───────────────────────────────────

pub(crate) use lists::debugfs_init;

pub mod lists {
use core::ffi::{c_void, CStr};
use core::ptr::NonNull;
use crate::data::{read::Rbio, update_task::Update};
use crate::debug::async_objs::{AsyncObj, AsyncObjList};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::os::debugfs::{DebugFile, DebugOut, Dir};
use crate::util::{place::Contained, rcu, task::TaskRef, Printbuf};

/// How a list's objects are shown: of the list's kind, read under RCU.
type ObjToText = unsafe fn(&mut Printbuf, &Fs, NonNull<c_void>);

// Each list's, named for it. SAFETY, for each: the caller's - but see the XXX

#[cold]
unsafe fn data_update_obj_to_text(out: &mut Printbuf, fs: &Fs, obj: NonNull<c_void>) {
    unsafe { obj.cast::<Update>().as_ref() }.to_text(out, fs)
}

#[cold]
unsafe fn rbio_obj_to_text(out: &mut Printbuf, _: &Fs, obj: NonNull<c_void>) {
    unsafe { obj.cast::<Rbio>().as_ref() }.to_text(out)
}

#[cold]
unsafe fn task_obj_to_text(out: &mut Printbuf, _: &Fs, obj: NonNull<c_void>) {
    unsafe { TaskRef::from_listed(obj) }.to_text(out)
}

macro_rules! list_table {
    ($(($name:ident, $nr:expr)),* $(,)?) => { crate::paste! {
        /// Each list's name, and how its objects are shown: indexed as
        /// async_objs[].
        const LISTS: [(&CStr, ObjToText); crate::debug::async_objs::AsyncObjKind::NR as usize] = [$((
            match CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()) {
                Ok(name) => name,
                Err(_)   => panic!(),
            },
            [<$name _obj_to_text>],
        )),*];
    }};
}
AsyncObjKind_arms!(list_table);

impl AsyncObjList {
    /// The filesystem it's one of the async_objs[] of.
    fn fs(&self) -> &Fs {
        self.container()
    }
}

enum AsyncObjListFile {}

impl DebugFile for AsyncObjListFile {
    type Obj = AsyncObjList;

    async fn show(list: &AsyncObjList, out: &mut DebugOut) -> Result<(), BchError> {
        let fs = list.fs();
        let (_, to_text) = LISTS[list.idx.get() as usize];

        let mut next = 0;
        out.print_each(|out| {
            let rcu = rcu::read_lock();
            let Some((idx, obj)) = list.list.items_from(next, &rcu).next() else {
                return Ok(false);
            };

            // SAFETY: an object of the list's kind - but see the XXX
            unsafe { to_text(out, fs, obj) };
            next = idx + 1;
            Ok(true)
        }).await
    }
}

/// debug_init()'s, in its directory @parent.
pub(crate) fn debugfs_init(fs: &Fs, parent: Dir<'_>) {
    let Some(dir) = parent.subdir(c"async_objs") else { return };
    for (list, (name, _)) in fs.async_objs.iter().zip(LISTS) {
        // SAFETY: debug_exit() removes them first
        unsafe { dir.file::<AsyncObjListFile>(name, list) };
    }
}
}
