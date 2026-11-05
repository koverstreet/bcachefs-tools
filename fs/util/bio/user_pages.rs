// SPDX-License-Identifier: GPL-2.0

//! A bio of the user's pages, pinned from an iov_iter - a direct IO's - and
//! given back with it.
//!
//! A read dirties the pages it reads into before it's submitted, and checks
//! them when it's done: one cleaned under it - by writeback - is dirtied
//! again, which takes its folio lock, so from a work item - the check is an
//! endio's. bio_set_pages_dirty() and bio_check_pages_dirty(), which 7.2
//! unexported.

use super::{Bio, BioBox, HasBio};
use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::init_at;
use crate::util::iov_iter::IovIter;
use crate::util::workqueue::{flush_work, Queue};
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_uint};
use core::pin::Pin;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering::{Acquire, Relaxed, Release}};
use cstruct_macros::{c_export, c_extern};

c_extern! {
    fn bch2_bio_iov_iter_get_pages(bio: *mut c::bio, iter: *mut c::iov_iter, align: c_uint) -> c_int;
}

impl Bio {
    /// Its bvecs the pages of @iter's next bytes, pinned, as many as it has
    /// room for - @iter advanced past them. Its box gives them back, with
    /// release_user_pages().
    pub fn pin_user_pages(self: Pin<&mut Self>, iter: &mut IovIter<'_>) -> Result<(), BchError> {
        let ret = unsafe { bch2_bio_iov_iter_get_pages(self.raw_mut(), iter.as_ptr(), 0) };
        if ret < 0 { Err(BchError::from_raw(ret)) } else { Ok(()) }
    }

    /// Its pages dirtied, before it's read into: the user's.
    pub fn set_pages_dirty(self: Pin<&mut Self>) {
        for f in self.into_ref().get_ref().range_all().folios() {
            let folio = f.folio.as_ptr();
            unsafe {
                c::folio_lock(folio);
                c::folio_mark_dirty(folio);
                c::folio_unlock(folio);
            }
        }
    }
}

impl<T: HasBio> BioBox<T> {
    /// The bio given back, with the user's pages it pinned - @dirtied, if
    /// set_pages_dirty() dirtied them, as they're checked dirty still:
    /// bio_check_or_release().
    pub fn release_user_pages(self, dirtied: bool) {
        let bio = self.into_bio();
        unsafe {
            if dirtied {
                bch2_bio_check_pages_dirty(bio);
            } else {
                c::bio_release_pages(bio, false);
                c::bio_put(bio);
            }
        }
    }
}

#[c_export]
pub unsafe fn bch2_bio_set_pages_dirty(bio: *mut c::bio) {
    unsafe { Bio::from_raw_mut(bio) }.set_pages_dirty()
}

/// @bio given back, its pages released - those cleaned since
/// set_pages_dirty() dirtied again first, from DIRTY_WORK.
#[c_export]
pub unsafe fn bch2_bio_check_pages_dirty(bio: *mut c::bio) {
    let all_dirty = unsafe { Bio::from_raw(bio) }.range_all().folios()
        .all(|f| unsafe { c::folio_test_dirty(f.folio.as_ptr()) });

    if all_dirty {
        unsafe {
            c::bio_release_pages(bio, false);
            c::bio_put(bio);
        }
        return;
    }

    let mut head = DIRTY.load(Relaxed);
    loop {
        unsafe { (*bio).bi_private = head.cast() };
        match DIRTY.compare_exchange_weak(head, bio, Release, Relaxed) {
            Ok(_)  => break,
            Err(h) => head = h,
        }
    }
    unsafe { Queue::system().queue_work(DIRTY_WORK.0.get()) };
}

/// Bios to dirty the pages of again, through bi_private: pushed from endios,
/// taken all at once by DIRTY_WORK.
static DIRTY: AtomicPtr<c::bio> = AtomicPtr::new(null_mut());

/// Initialized at module init, flushed at exit.
struct DirtyWork(UnsafeCell<c::work_struct>);

unsafe impl Sync for DirtyWork {}

static DIRTY_WORK: DirtyWork = DirtyWork(UnsafeCell::new(unsafe { core::mem::zeroed() }));

unsafe extern "C" fn dirty_fn(_: *mut c::work_struct) {
    let mut next = DIRTY.swap(null_mut(), Acquire);
    while let Some(bio) = NonNull::new(next) {
        let bio = bio.as_ptr();
        unsafe {
            next = (*bio).bi_private.cast();
            c::bio_release_pages(bio, true);
            c::bio_put(bio);
        }
    }
}

pub(crate) fn dirty_pages_init() {
    let Ok(()) = unsafe { init_at(crate::new_work!(dirty_fn, "bio_dirty"), DIRTY_WORK.0.get()) };
}

/// After the last direct IO: none left to queue it.
pub(crate) fn dirty_pages_exit() {
    unsafe { flush_work(DIRTY_WORK.0.get()) }
}
