// SPDX-License-Identifier: GPL-2.0

//! A bio of the user's pages, pinned from an iov_iter - a direct IO's - and
//! given back with it.

use super::{Bio, BioBox, HasBio};
use crate::c;
use crate::errcode::BchError;
use crate::util::iov_iter::IovIter;
use core::ffi::{c_int, c_uint};
use core::pin::Pin;
use cstruct_macros::c_extern;

c_extern! {
    fn bch2_bio_iov_iter_get_pages(bio: *mut c::bio, iter: *mut c::iov_iter, align: c_uint) -> c_int;
    fn bch2_bio_set_pages_dirty(bio: *mut c::bio);
    fn bch2_bio_check_pages_dirty(bio: *mut c::bio);
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
        unsafe { bch2_bio_set_pages_dirty(self.raw_mut()) }
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
