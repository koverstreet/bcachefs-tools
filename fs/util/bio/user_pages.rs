// SPDX-License-Identifier: GPL-2.0

//! A bio of the user's pages, pinned from an iov_iter - a direct IO's - and
//! given back with it.
//!
//! A read dirties the pages it reads into before it's submitted, and checks
//! them when it's done: one cleaned under it - by writeback - is dirtied
//! again, which takes its folio lock, so the read's task does it from process
//! context, requeued there if the check was an endio's.
//! bio_set_pages_dirty() and bio_check_pages_dirty(), which 7.2 unexported.

use super::Bio;
use crate::c;
use crate::errcode::BchError;
use crate::util::iov_iter::IovIter;
use crate::util::task::requeue;
use crate::util::workqueue::Queue;
use core::ffi::{c_int, c_uint};
use core::pin::Pin;
use cstruct_macros::c_export;

use crate::c::bch2_bio_iov_iter_get_pages;

impl Bio {
    /// Its bvecs the pages of @iter's next bytes, pinned, as many as it has
    /// room for - @iter advanced past them. release_user_pages() gives them
    /// back.
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

    /// The user's pages it pinned, given back - @dirtied, if set_pages_dirty()
    /// dirtied them, as they're checked dirty still: those cleaned since are
    /// dirtied again first, under their folio locks.
    pub async fn release_user_pages(mut self: Pin<&mut Self>, dirtied: bool) {
        let redirty = dirtied && !self.as_ref().get_ref().range_all().folios()
            .all(|f| unsafe { c::folio_test_dirty(f.folio.as_ptr()) });
        if redirty {
            requeue(Queue::system_dfl()).await;
        }
        unsafe { c::bio_release_pages(self.as_mut().raw_mut(), redirty) };
    }
}

#[c_export]
pub unsafe fn bch2_bio_set_pages_dirty(bio: *mut c::bio) {
    unsafe { Bio::from_raw_mut(bio) }.set_pages_dirty()
}
