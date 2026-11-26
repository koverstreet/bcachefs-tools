// SPDX-License-Identifier: GPL-2.0

//! A page cache folio's bcachefs state - bch_folio, vfs/pagecache.c's; the
//! folios, the page cache and its operations are os::folio's, typed by
//! InodeInfo, whose address space operations they have.
//!
//! A folio's state is created on it before it's read into, and
//! partially_uptodate is the read's to keep; the rest is under the state's
//! lock, lock_state()'s.

use super::inode::InodeInfo;
use crate::c;
use crate::vfs::pagecache::{FolioSector, FolioState};
use crate::errcode::BchError;
use crate::locking::{ExclusiveGuard, SpinLockIrq};
use crate::os::{folio, bio::FolioHold};
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic::Ordering::{AcqRel, Relaxed};

pub type Folio = folio::Folio<InodeInfo>;
pub type FolioGuard<'a> = folio::FolioGuard<'a, InodeInfo>;
pub type FolioLocked = folio::FolioLocked<InodeInfo>;
pub type FolioSpan<'a> = folio::FolioSpan<'a, InodeInfo>;
pub type HeldFolio<'a> = folio::HeldFolio<'a, InodeInfo>;
pub type LockedFolio = folio::LockedFolio<InodeInfo>;
pub type Mapping = folio::Mapping<InodeInfo>;
pub type Readahead<'a> = folio::Readahead<'a, InodeInfo>;

impl FolioLocked {
    /// Its data read in, @mapping's, before this returns - and with @extend,
    /// the read extended as readahead's are.
    pub fn read_single(&self, mapping: Mapping, extend: bool) -> Result<(), BchError> {
        super::buffered::read_single_folio(self, mapping, extend)
    }
}

impl Folio {
    /// Panics if it has no bcachefs state.
    pub fn partially_uptodate(&self) -> u16 {
        self.private().expect("a folio's bch_folio").partially_uptodate.load(Relaxed)
    }

    pub fn set_partially_uptodate(&self, sectors: u16) {
        self.private().expect("a folio's bch_folio").partially_uptodate.store(sectors, Relaxed)
    }

    /// Its bcachefs state, locked, if it has any.
    pub fn lock_state_if_any(&self) -> Option<FolioStateGuard<'_>> {
        Some(FolioStateGuard { guard: self.private()?.state.lock(), sectors: self.nr_sectors() })
    }

    /// Its bcachefs state, locked: its caller made it first. Panics if it
    /// has none.
    pub fn lock_state(&self) -> FolioStateGuard<'_> {
        self.lock_state_if_any().expect("a folio's bch_folio")
    }
}

/// A folio's bcachefs state, its lock held.
pub struct FolioStateGuard<'a> {
    guard:   ExclusiveGuard<'a, SpinLockIrq, FolioState>,
    sectors: usize,
}

impl Deref for FolioStateGuard<'_> {
    type Target = FolioState;

    fn deref(&self) -> &FolioState {
        &self.guard
    }
}

impl DerefMut for FolioStateGuard<'_> {
    fn deref_mut(&mut self) -> &mut FolioState {
        &mut self.guard
    }
}

impl FolioStateGuard<'_> {
    /// s[], a sector of the folio each.
    pub fn sectors(&mut self) -> &mut [FolioSector] {
        unsafe { core::slice::from_raw_parts_mut((&raw mut self.guard.s).cast(), self.sectors) }
    }
}

impl FolioGuard<'_> {
    /// Under writeback, and unlocked: folio_start_writeback(), then
    /// folio_unlock(). Panics if it has no bcachefs state, or writes in
    /// flight.
    pub fn start_writeback(self) -> Writeback {
        let s = self.private().expect("a folio written back with no bch_folio");
        assert!(s.write_count.load(Relaxed) == 0, "a folio with writes in flight, written back again");
        assert!(!self.writeback(), "a folio already under writeback, written back again");
        s.write_count.store(1, Relaxed);

        let folio = self.keep_locked();
        unsafe {
            c::rust_folio_start_writeback(folio.as_ptr());
            c::folio_unlock(folio.as_ptr());
            Writeback(Folio::from_raw(folio))
        }
    }
}

/// A count on a folio's writeback, write_count's - which keeps it in the
/// page cache, as a lock does: start_writeback()'s, and one for each io it's
/// added to, share()'s. Its writeback is ended when the last is dropped.
pub struct Writeback(&'static Folio);

impl Writeback {
    /// A count for an io the folio's added to.
    pub fn share(&self) -> Writeback {
        self.private().expect("a folio in writeback with no bch_folio").write_count.fetch_add(1, Relaxed);
        Writeback(self.0)
    }
}

// ended by whichever thread finishes the io it's in
unsafe impl Send for Writeback {}

// a count on its writeback, which keeps it
unsafe impl FolioHold for Writeback {
    type A = InodeInfo;

    fn folio(&self) -> NonNull<c::folio> {
        NonNull::from(self.0).cast()
    }

    unsafe fn release(folio: NonNull<c::folio>, _: usize, _: usize) {
        drop(Writeback(unsafe { Folio::from_raw(folio) }));
    }
}

impl Deref for Writeback {
    type Target = Folio;

    fn deref(&self) -> &Folio {
        self.0
    }
}

impl Drop for Writeback {
    fn drop(&mut self) {
        let s = self.private().expect("a folio in writeback with no bch_folio");
        if s.write_count.fetch_sub(1, AcqRel) == 1 {
            unsafe { c::folio_end_writeback(self.0.as_ptr()) }
        }
    }
}

impl FolioGuard<'static> {
    /// Its lock the read's, held by the read's count. Panics if it has no
    /// bcachefs state, or a read in flight.
    pub fn start_read(self) -> ReadPart {
        let s = self.private().expect("a folio read with no bch_folio");
        assert!(s.read_count.load(Relaxed) == 0, "a folio with a read in flight, read again");
        s.read_count.store(1, Relaxed);
        ReadPart(unsafe { Folio::from_raw(self.keep_locked()) })
    }
}

/// A count on a folio's read, read_count's: start_read()'s, and one for each
/// extent's bio it's in, share()'s. The last ends the read: uptodate if the
/// window covered it and no extent's read of it failed.
pub struct ReadPart(&'static Folio);

impl ReadPart {
    /// A read of @folio, whose lock stays the caller's: the last count
    /// leaves it locked, and not uptodate - the caller marks it, from what
    /// the read returned. Panics as start_read().
    ///
    /// # Safety
    /// The caller holds @folio locked until every count is dropped: it waits
    /// for each extent's read it's in.
    pub unsafe fn callers(folio: &FolioLocked) -> ReadPart {
        let s = folio.private().expect("a folio read with no bch_folio");
        assert!(s.read_count.load(Relaxed) == 0, "a folio with a read in flight, read again");
        s.read_count.store(1, Relaxed);
        s.read_callers.store(true, Relaxed);
        ReadPart(unsafe { Folio::from_raw(NonNull::from(folio).cast()) })
    }

    /// A count for an extent's bio the folio's in.
    pub fn share(&self) -> ReadPart {
        self.private().expect("a folio read with no bch_folio").read_count.fetch_add(1, Relaxed);
        ReadPart(self.0)
    }

    /// The window handed out all of it, from read_from on.
    pub fn covered(&self) {
        self.private().expect("a folio read with no bch_folio").read_covered.store(true, Relaxed);
    }
}

impl Folio {
    /// An extent's read of part of it failed: its read ends not uptodate.
    pub fn read_failed(&self) {
        self.private().expect("a folio read with no bch_folio").read_failed.store(true, Relaxed);
    }
}

// ended by whichever thread finishes the last extent's read it's in
unsafe impl Send for ReadPart {}

// a count on its read, whose lock keeps it
unsafe impl FolioHold for ReadPart {
    type A = InodeInfo;

    fn folio(&self) -> NonNull<c::folio> {
        NonNull::from(self.0).cast()
    }

    unsafe fn release(folio: NonNull<c::folio>, _: usize, _: usize) {
        drop(ReadPart(unsafe { Folio::from_raw(folio) }));
    }
}

impl Deref for ReadPart {
    type Target = Folio;

    fn deref(&self) -> &Folio {
        self.0
    }
}

impl Drop for ReadPart {
    fn drop(&mut self) {
        let s = self.private().expect("a folio read with no bch_folio");
        if s.read_count.fetch_sub(1, AcqRel) != 1 {
            return;
        }
        let failed = s.read_failed.swap(false, Relaxed);
        let uptodate = s.read_covered.swap(false, Relaxed) && !failed;
        /* what partially_uptodate says was read may be what failed: */
        if uptodate || failed {
            s.partially_uptodate.store(0, Relaxed);
        }
        if !s.read_callers.swap(false, Relaxed) {
            unsafe { FolioGuard::adopt(NonNull::from(self.0).cast()) }.end_read(uptodate);
        }
    }
}

impl Mapping {
    /// Its pages from @start to @end, inclusive, written and dropped:
    /// pagecache_block must be held.
    pub fn write_invalidate_range(self, start: u64, end: u64) -> Result<(), BchError> {
        /*
         * XXX: the way this is currently implemented, we can spin if a process
         * is continually redirtying a specific page
         */
        loop {
            if self.is_empty() {
                return Ok(());
            }

            self.write_and_wait_range(start, end)?;

            if self.is_empty() {
                return Ok(());
            }

            match self.invalidate_range(start, end) {
                Err(e) if e.matches(c::EBUSY) => {}
                r => return r,
            }
        }
    }
}
