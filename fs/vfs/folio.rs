// SPDX-License-Identifier: GPL-2.0

//! A page cache folio's bcachefs state - bch_folio, vfs/pagecache.c's - and a
//! mapping's operations; the folios and the page cache are os::folio's.
//!
//! A folio's state is created on it before it's read into, and
//! partially_uptodate is the read's to keep; the rest is under the state's
//! lock, lock_state()'s.

use super::buffered_write::WriteBegun;
use super::inode::InodeInfo;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::locking::{ExclusiveGuard, SpinLockIrq};
use crate::os::folio::{Folio, FolioGuard, LockedFolio, Mapping, Readahead, VfsWriter, Wbc};
use crate::util::alloc::KBox;
use core::ffi::{c_int, c_uint, c_void};
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic::Ordering::{AcqRel, Relaxed};
use kernel::page::PAGE_SHIFT;

impl Folio {
    /// Its data read in, @mapping's, before this returns - and with @extend,
    /// the read extended as readahead's are.
    pub fn read_single(self, mapping: Mapping, extend: bool) -> Result<(), BchError> {
        super::buffered::read_single_folio(self, mapping, extend)
    }

    pub(super) fn state_ptr(self) -> Option<NonNull<c::bch_folio>> {
        NonNull::new(unsafe { c::folio_get_private(self.as_ptr()) }.cast())
    }

    /// Its bcachefs state, if it has any: what's atomic of it - the rest is
    /// lock_state()'s.
    pub(super) fn state(&self) -> Option<&c::bch_folio> {
        self.state_ptr().map(|s| unsafe { s.as_ref() })
    }

    /// Panics if it has no bcachefs state.
    pub fn partially_uptodate(self) -> u16 {
        unsafe { (*self.state_ptr().expect("a folio's bch_folio").as_ptr()).partially_uptodate }
    }

    pub fn set_partially_uptodate(self, sectors: u16) {
        unsafe { (*self.state_ptr().expect("a folio's bch_folio").as_ptr()).partially_uptodate = sectors }
    }

    /// Its bcachefs state, locked, if it has any.
    pub fn lock_state_if_any(&self) -> Option<FolioStateGuard<'_>> {
        Some(FolioStateGuard { guard: self.state()?.state.lock(), sectors: self.nr_sectors() })
    }

    /// Its bcachefs state, locked: its caller made it first. Panics if it
    /// has none.
    pub fn lock_state(&self) -> FolioStateGuard<'_> {
        self.lock_state_if_any().expect("a folio's bch_folio")
    }
}

/// A folio's bcachefs state, its lock held.
pub struct FolioStateGuard<'a> {
    guard:   ExclusiveGuard<'a, SpinLockIrq, c::FolioState>,
    sectors: usize,
}

impl Deref for FolioStateGuard<'_> {
    type Target = c::FolioState;

    fn deref(&self) -> &c::FolioState {
        &self.guard
    }
}

impl DerefMut for FolioStateGuard<'_> {
    fn deref_mut(&mut self) -> &mut c::FolioState {
        &mut self.guard
    }
}

impl FolioStateGuard<'_> {
    /// s[], a sector of the folio each.
    pub fn sectors(&mut self) -> &mut [c::bch_folio_sector] {
        unsafe { core::slice::from_raw_parts_mut((&raw mut self.guard.s).cast(), self.sectors) }
    }
}

impl FolioGuard<'_> {
    /// Under writeback, and unlocked: folio_start_writeback(), then
    /// folio_unlock(). Panics if it has no bcachefs state, or writes in
    /// flight.
    pub fn start_writeback(self) -> Writeback {
        let s = self.state().expect("a folio written back with no bch_folio");
        assert!(s.write_count.load(Relaxed) == 0, "a folio with writes in flight, written back again");
        assert!(!self.writeback(), "a folio already under writeback, written back again");
        s.write_count.store(1, Relaxed);

        let folio = self.keep_locked();
        unsafe {
            c::rust_folio_start_writeback(folio.as_ptr());
            c::folio_unlock(folio.as_ptr());
        }
        Writeback(folio)
    }
}

/// A count on a folio's writeback, write_count's - which keeps it in the
/// page cache, as a lock does: start_writeback()'s, and one for each io it's
/// added to, share()'s. Its writeback is ended when the last is dropped.
pub struct Writeback(Folio);

impl Writeback {
    /// A count for an io the folio's added to, given back by from_io().
    pub fn share(&self) {
        self.state().expect("a folio in writeback with no bch_folio").write_count.fetch_add(1, Relaxed);
    }

    /// An io's count, share()'s, from the io's bio as it ends.
    ///
    /// # Safety
    /// @folio is one share() counted for this io, taken back once.
    pub unsafe fn from_io(folio: NonNull<c::folio>) -> Self {
        Writeback(unsafe { Folio::from_raw(folio.as_ptr()) })
    }
}

impl Deref for Writeback {
    type Target = Folio;

    fn deref(&self) -> &Folio {
        &self.0
    }
}

impl Drop for Writeback {
    fn drop(&mut self) {
        let s = self.state().expect("a folio in writeback with no bch_folio");
        if s.write_count.fetch_sub(1, AcqRel) == 1 {
            unsafe { c::folio_end_writeback(self.0.as_ptr()) }
        }
    }
}

impl Mapping {
    pub fn inode(&self) -> &InodeInfo {
        unsafe { InodeInfo::from_inode((*self.as_ptr()).host) }
    }

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

            match ret_to_result_void(unsafe {
                c::invalidate_inode_pages2_range(self.as_ptr(), (start >> PAGE_SHIFT) as _, (end >> PAGE_SHIFT) as _)
            }) {
                Err(e) if e.matches(c::EBUSY) => {}
                r => return r,
            }
        }
    }
}

crate::vfs_ops! {
    /// A mapping's operations, address_space_operations.
    pub(crate) trait AddressSpaceOps for c::address_space_operations, AopsVTable<T: AddressSpaceOps> = T;

    /// @folio's data read in; its lock, the VFS's, given up either way.
    fn read_folio(_file: *mut c::file, folio: FolioGuard<'_>) -> Result<(), BchError>;
    fn readahead(ra: Readahead<'_>);
    fn writepages(mapping: Mapping, wbc: Wbc) -> c_int;
    /// @folio marked dirty: whether it wasn't.
    fn dirty_folio(mapping: Mapping, folio: Folio) -> bool;
    /// The folio @len bytes at @pos are written into, locked, in *@foliop,
    /// and what write_end() has of the write, in *@fsdata.
    fn write_begin(_w: VfsWriter, mapping: Mapping, pos: c::loff_t, len: c_uint,
                   foliop: &mut *mut c::folio, fsdata: &mut *mut c_void) -> Result<(), BchError>;
    /// @copied of the @len bytes at @pos written into @folio, write_begin()'s:
    /// how many were.
    fn write_end(_w: VfsWriter, mapping: Mapping, pos: c::loff_t, len: c_uint, copied: c_uint,
                 folio: LockedFolio, begun: KBox<WriteBegun>) -> c_int;
    fn invalidate_folio(folio: Folio, offset: usize, len: usize);
    /// Whether @folio's bcachefs state was freed, for the folio to be.
    fn release_folio(folio: Folio, gfp: c::gfp_t) -> bool;
}
