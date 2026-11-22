// SPDX-License-Identifier: GPL-2.0

//! A bio of the user's pages, pinned from an iov_iter - a direct IO's - and
//! given back with it.
//!
//! A read dirties the pages it reads into before it's submitted, and checks
//! them when it's done: one cleaned under it - by writeback - is dirtied
//! again, which takes its folio lock, so the read's task does it from process
//! context, requeued there if the check was an endio's.
//! bio_set_pages_dirty() and bio_check_pages_dirty(), which 7.2 unexported.

use super::iov_iter::IovIter;
use super::kernel::PAGE_SIZE;
use crate::c;
use crate::errcode::BchError;
use crate::os::file::Any;
use crate::os::folio::Folio;
use crate::util::bio::{Bio, Bvec, FolioHold, Holds};
use crate::util::task::requeue;
use crate::os::workqueue::Queue;
use core::ffi::{c_int, c_uint, c_ulong};
use core::ptr::NonNull;

/// What a bio of an iov_iter's pages holds of their folios: Pinned, a
/// user-backed iter's - iov_iter_extract_will_pin() - or Callers'.
pub trait UserPage: FolioHold<A = Any> + Send + 'static {
    const PINNED: bool;

    /// # Safety
    /// @folio's pages are ones iov_iter_extract_pages() gave, of an iter
    /// whose pages it pins as PINNED says.
    unsafe fn extracted(folio: NonNull<c::folio>) -> Self;
}

/// A user folio, the pages of it a bvec has pinned: unpinned as it's released.
pub struct Pinned(NonNull<c::folio>);

/// A folio of the caller's - a bvec or kvec iter's - which it keeps till the
/// IO that has it is done.
pub struct Callers(NonNull<c::folio>);

// a folio's, for whichever thread ends the IO
unsafe impl Send for Pinned {}
unsafe impl Send for Callers {}

unsafe impl FolioHold for Pinned {
    type A = Any;

    fn folio(&self) -> NonNull<c::folio> {
        self.0
    }

    unsafe fn release(folio: NonNull<c::folio>, offset: usize, len: usize) {
        let pages = (offset + len).div_ceil(PAGE_SIZE) - offset / PAGE_SIZE;
        unsafe { c::unpin_user_folio(folio.as_ptr(), pages as c_ulong) }
    }
}

unsafe impl FolioHold for Callers {
    type A = Any;

    fn folio(&self) -> NonNull<c::folio> {
        self.0
    }

    unsafe fn release(_: NonNull<c::folio>, _: usize, _: usize) {}
}

impl UserPage for Pinned {
    const PINNED: bool = true;

    unsafe fn extracted(folio: NonNull<c::folio>) -> Self {
        Pinned(folio)
    }
}

impl UserPage for Callers {
    const PINNED: bool = false;

    unsafe fn extracted(folio: NonNull<c::folio>) -> Self {
        Callers(folio)
    }
}

/// Pages extracted at a time: the room on the stack for their pointers.
const EXTRACT_BATCH: usize = 16;

/*
 * We might COW a single page in the middle of
 * a large folio, so we have to check that all
 * pages belong to the same folio.
 */
/// How many of @pages from the first are a run of one folio, @folio, and how
/// many of @left bytes they hold, the first from @offset.
fn contig_folio_len(pages: &[*mut c::page], folio: *mut c::folio, mut left: usize, offset: usize) -> (usize, usize) {
    let mut len = (PAGE_SIZE - offset).min(left);
    left -= len;
    let mut j = 1;
    while j < pages.len() && unsafe { c::rust_page_folio(pages[j]) } == folio &&
          pages[j] == pages[j - 1].wrapping_add(1) {
        let next = PAGE_SIZE.min(left);
        len += next;
        left -= next;
        j += 1;
    }
    (j, len)
}

impl<P: UserPage> Bio<Holds<P>> {
    /// Its bvecs @iter's next pages, a run of one folio a bvec, as many as it
    /// has room for - @iter advanced past them. Fails only if it got none.
    /// Panics if @iter's pages aren't P's.
    pub fn extract_pages(&mut self, iter: &mut IovIter<'_>) -> Result<(), BchError> {
        assert_eq!(iter.user_backed(), P::PINNED, "a bio of {} pages, of an iter whose pages aren't",
                   if P::PINNED { "pinned" } else { "the caller's" });
        let start = self.nr_vecs();

        while iter.count() != 0 && self.nr_vecs() < self.max_vecs() {
            let mut pages = [core::ptr::null_mut::<c::page>(); EXTRACT_BATCH];
            let mut p = pages.as_mut_ptr();
            let mut offset = 0;
            let room = usize::from(self.max_vecs() - self.nr_vecs()).min(EXTRACT_BATCH);
            let max = (u32::MAX - self.raw().bi_iter.bi_size) as usize;

            let got = unsafe { c::iov_iter_extract_pages(iter.as_ptr(), &mut p, max, room as c_uint, 0, &mut offset) };
            if got <= 0 {
                if self.nr_vecs() != start {
                    break;
                }
                return Err(if got < 0 { BchError::from_raw(got as c_int) } else { c::EFAULT.into() });
            }

            let (mut left, mut i) = (got as usize, 0);
            let pages = &pages[..(offset + left).div_ceil(PAGE_SIZE)];
            while left != 0 {
                let (page, folio) = (pages[i], unsafe { c::rust_page_folio(pages[i]) });
                let folio_offset = unsafe { c::rust_folio_page_idx(folio, page) } as usize * PAGE_SIZE + offset;
                let mut len = (unsafe { c::folio_size(folio) } - folio_offset).min(left);
                let mut n = (offset + len).div_ceil(PAGE_SIZE);
                if n > 1 {
                    (n, len) = contig_folio_len(&pages[i..i + n], folio, left, offset);
                }

                let held = unsafe { P::extracted(NonNull::new_unchecked(folio)) };
                self.push(Bvec::new(held, folio_offset, len)).expect("extracting pages: no more than it has room for");
                left -= len;
                i += n;
                offset = 0;
            }
        }
        Ok(())
    }
}

/// @folio dirtied, under its lock: folio_mark_dirty_lock().
fn mark_dirty(folio: &Folio) {
    unsafe {
        c::folio_lock(folio.as_ptr());
        c::folio_mark_dirty(folio.as_ptr());
        c::folio_unlock(folio.as_ptr());
    }
}

impl<P: UserPage> Bio<Holds<P>> {
    /// Its pages dirtied, before it's read into: the user's.
    pub fn set_pages_dirty(&mut self) {
        for v in self.vecs() {
            mark_dirty(v.folio());
        }
    }

    /// Its pages given back - @dirtied, if set_pages_dirty() dirtied them, as
    /// they're checked dirty still: those cleaned since are dirtied again
    /// first, under their folio locks, as bio_release_pages() does.
    pub async fn release_user_pages(&mut self, dirtied: bool) {
        if dirtied && !self.vecs().iter().all(|v| v.folio().dirty()) {
            requeue(Queue::system_dfl()).await;
            for v in self.vecs().iter().filter(|v| !v.folio().dirty()) {
                mark_dirty(v.folio());
            }
        }
        self.release();
    }
}
