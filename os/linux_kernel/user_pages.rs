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
use crate::errcode::{BchError, Errcode};
use crate::os::{file::Any, folio::Folio, bio::{Bio, Bvec, FolioHold, Holds}, workqueue::Queue};
use crate::util::{rcu::{DropAnytime, DropAtomic}, task::requeue};
use core::ffi::{c_int, c_uint, c_ulong};
use core::ptr::NonNull;

/// What a bio of an iov_iter's pages holds of their folios: Pinned, a
/// user-backed iter's - iov_iter_extract_will_pin() - or Callers'.
pub trait UserPage: FolioHold<A = Any> + Send + DropAtomic + DropAnytime + 'static {
    /// @bio's bvecs @iter's next pages, a run of one folio a bvec, as many as
    /// it has room for - @iter advanced past them. Fails only if it got none.
    /// Panics if @iter's pages aren't held as Self holds them.
    fn extract_pages(bio: &mut Bio<Holds<Self>>, iter: &mut IovIter<'_>) -> Result<(), BchError>;
}

/// A user folio, the pages of it a bvec has: PINNED, a user-backed iter's,
/// unpinned as it's released; or the caller's - a bvec or kvec iter's -
/// which it keeps till the IO that has it is done. Made only as its pages
/// are extracted.
pub struct UserFolio<const PINNED: bool>(NonNull<c::folio>);

pub type Pinned = UserFolio<true>;
pub type Callers = UserFolio<false>;

// a folio's, for whichever thread ends the IO
unsafe impl<const PINNED: bool> Send for UserFolio<PINNED> {}

unsafe impl<const PINNED: bool> FolioHold for UserFolio<PINNED> {
    type A = Any;

    fn folio(&self) -> NonNull<c::folio> {
        self.0
    }

    unsafe fn release(folio: NonNull<c::folio>, offset: usize, len: usize) {
        if PINNED {
            let pages = (offset + len).div_ceil(PAGE_SIZE) - offset / PAGE_SIZE;
            unsafe { c::unpin_user_folio(folio.as_ptr(), pages as c_ulong) }
        }
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

impl<const PINNED: bool> UserPage for UserFolio<PINNED> {
    fn extract_pages(bio: &mut Bio<Holds<Self>>, iter: &mut IovIter<'_>) -> Result<(), BchError> {
        assert_eq!(iter.user_backed(), PINNED, "a bio of {} pages, of an iter whose pages aren't",
                   if PINNED { "pinned" } else { "the caller's" });
        let start = bio.nr_vecs();

        while iter.count() != 0 && bio.nr_vecs() < bio.max_vecs() {
            let mut pages = [core::ptr::null_mut::<c::page>(); EXTRACT_BATCH];
            let mut p = pages.as_mut_ptr();
            let mut offset = 0;
            let room = usize::from(bio.max_vecs() - bio.nr_vecs()).min(EXTRACT_BATCH);
            let max = (u32::MAX - bio.range().len()) as usize;

            let got = unsafe { c::iov_iter_extract_pages(iter.as_ptr(), &mut p, max, room as c_uint, 0, &mut offset) };
            if got <= 0 {
                if bio.nr_vecs() != start {
                    break;
                }
                return Err(if got < 0 { BchError::from_raw(got as c_int) } else { Errcode::EFAULT_extract_pages.into() });
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

                let held = UserFolio(NonNull::new(folio).expect("an extracted page's folio"));
                bio.push(Bvec::new(held, folio_offset, len)).expect("extracting pages: no more than it has room for");
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
