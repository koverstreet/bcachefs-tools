// SPDX-License-Identifier: GPL-2.0

//! The page cache, as a read fills it.
//!
//! A Folio is a page cache folio the caller holds locked, as a read holds the
//! folios it fills: the lock keeps it in the page cache, so a Folio is Copy
//! and takes no ref.
//!
//! A LockedFolio is one looked up locked and with a ref, both its own: a
//! Folio until it's dropped, which unlocks and puts it. A FolioRef is one
//! not locked, kept by a ref someone else has - a walk's batch, walk_folios()'s,
//! or a fault's page - for its holder to lock: a FolioGuard, a Folio until
//! it's dropped, or until it's given up locked, keep_locked().
//!
//! A Mapping is an inode's page cache: where a read finds and adds folios - a
//! NewFolio, alloc_folio()'s, with a ref of its own till it's in. A Readahead
//! is the VFS's readahead_control: the locked folios it asks a read of, given
//! back - unread - by unread().

use crate::c;
use crate::errcode::{errptr_to_result, ret_to_result_void, BchError};
use crate::os::iov_iter::IovIter;
use crate::os::kernel::{cond_resched, loff};
use crate::os::ops::from_raw;
use core::borrow::Borrow;
use core::ffi::{c_int, c_void};
use core::marker::PhantomData;
use core::ops::{ControlFlow, Deref};
use core::ptr::NonNull;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Folio(NonNull<c::folio>);

impl Folio {
    /// # Safety
    /// @folio is locked by the caller, for as long as it uses this.
    pub unsafe fn from_raw(folio: *mut c::folio) -> Self {
        Folio(NonNull::new(folio).expect("a folio"))
    }

    pub fn as_ptr(self) -> *mut c::folio {
        self.0.as_ptr()
    }

    pub fn size(self) -> usize {
        unsafe { c::folio_size(self.as_ptr()) }
    }

    pub fn nr_sectors(self) -> usize {
        self.size() >> 9
    }

    pub fn nr_pages(self) -> usize {
        unsafe { c::folio_nr_pages(self.as_ptr()) as usize }
    }

    pub fn pos(self) -> u64 {
        unsafe { c::folio_pos(self.as_ptr()) as u64 }
    }

    pub fn end_pos(self) -> u64 {
        self.pos() + self.size() as u64
    }

    pub fn sector(self) -> u64 {
        self.pos() >> 9
    }

    pub fn end_sector(self) -> u64 {
        self.sector() + (self.size() >> 9) as u64
    }

    pub fn uptodate(self) -> bool {
        unsafe { c::folio_test_uptodate(self.as_ptr()) }
    }

    pub fn dirty(self) -> bool {
        unsafe { c::folio_test_dirty(self.as_ptr()) }
    }

    pub fn writeback(self) -> bool {
        unsafe { c::folio_test_writeback(self.as_ptr()) }
    }

    pub fn locked(self) -> bool {
        unsafe { c::folio_test_locked(self.as_ptr()) }
    }

    /// Its mapping, unless it's been truncated out of it.
    pub fn mapping(self) -> Option<Mapping> {
        NonNull::new(unsafe { c::rust_folio_mapping(self.as_ptr()) }).map(Mapping)
    }

    /// Waited for, if its data is being written and mustn't change meanwhile:
    /// folio_wait_stable().
    pub fn wait_stable(self) {
        unsafe { c::folio_wait_stable(self.as_ptr()) }
    }

    pub fn mark_uptodate(self) {
        unsafe { c::folio_mark_uptodate(self.as_ptr()) }
    }

    /// Bytes [@start, @end) of it zeroed: folio_zero_segment().
    pub fn zero_segment(self, start: usize, end: usize) {
        unsafe { c::folio_zero_segment(self.as_ptr(), start, end) }
    }

    /// Bytes [@start1, @end1) and [@start2, @end2) of it zeroed:
    /// folio_zero_segments().
    pub fn zero_segments(self, start1: usize, end1: usize, start2: usize, end2: usize) {
        unsafe { c::folio_zero_segments(self.as_ptr(), start1, end1, start2, end2) }
    }

    /// @len bytes of it from @start zeroed: folio_zero_range().
    pub fn zero_range(self, start: usize, len: usize) {
        unsafe { c::folio_zero_range(self.as_ptr(), start, len) }
    }

    /// What's written to it through the kernel's mapping made visible through
    /// userspace's: flush_dcache_folio().
    pub fn flush_dcache(self) {
        unsafe { c::flush_dcache_folio(self.as_ptr()) }
    }

    /// Its writeable userspace mappings write-protected, so a write through
    /// one faults - page_mkwrite again: folio_mkclean().
    pub fn mkclean(self) {
        unsafe { c::folio_mkclean(self.as_ptr()) };
    }

    /// Unlocked: the caller's lock, and with it the Folio, given up.
    pub fn end_read(self, uptodate: bool) {
        unsafe { c::folio_end_read(self.as_ptr(), uptodate) }
    }

    /// What its private holds detached and freed - kmalloc'd, as the
    /// filesystem's state on it is.
    pub fn free_private(self) {
        unsafe { c::kfree(c::folio_detach_private(self.as_ptr())) }
    }
}

/// @len bytes of a folio, from @offset into it.
#[derive(Clone, Copy)]
pub struct FolioSpan {
    pub folio:  Folio,
    pub offset: usize,
    pub len:    usize,
}

impl FolioSpan {
    /// @folio's part of the file's [@start, @end).
    pub fn new(folio: Folio, start: u64, end: u64) -> Self {
        let start = start.max(folio.pos());
        FolioSpan {
            folio,
            offset: (start - folio.pos()) as usize,
            len:    (end.min(folio.end_pos()) - start) as usize,
        }
    }

    pub fn whole(folio: Folio) -> Self {
        FolioSpan { folio, offset: 0, len: folio.size() }
    }

    /// Where it starts in the file.
    pub fn pos(self) -> u64 {
        self.folio.pos() + self.offset as u64
    }

    /// @iter copied in, without faulting: how much was -
    /// copy_folio_from_iter_atomic().
    pub fn copy_from_iter_atomic(self, iter: &mut IovIter<'_>) -> usize {
        unsafe { c::copy_folio_from_iter_atomic(self.folio.as_ptr(), self.offset, self.len, iter.as_ptr()) }
    }
}

/// A walk's batch of folios, a ref on each: filemap_get_folios()'s, put when
/// it's released or dropped.
struct FolioBatch(c::folio_batch);

impl FolioBatch {
    fn new() -> Self {
        let mut b = FolioBatch(::pin_init::zeroed());
        unsafe { c::folio_batch_init(&mut b.0) };
        b
    }

    fn folios(&self) -> &[*mut c::folio] {
        let nr = unsafe { c::folio_batch_count((&raw const self.0).cast_mut()) } as usize;
        &self.0.folios[..nr]
    }

    fn release(&mut self) {
        unsafe { c::folio_batch_release(&mut self.0) }
    }
}

impl Drop for FolioBatch {
    fn drop(&mut self) {
        self.release()
    }
}

/// A folio not locked, kept by a ref someone else has for 'r.
pub struct FolioRef<'r>(Folio, PhantomData<&'r ()>);

impl FolioRef<'_> {
    /// # Safety
    /// @folio is kept, by a ref of the caller's or its caller's, for 'r.
    pub unsafe fn from_raw(folio: *mut c::folio) -> Self {
        FolioRef(Folio(NonNull::new(folio).expect("a folio")), PhantomData)
    }

    pub fn pos(&self) -> u64 {
        self.0.pos()
    }

    pub fn size(&self) -> usize {
        self.0.size()
    }

    pub fn lock(&self) -> FolioGuard<'_> {
        unsafe { c::folio_lock(self.0.as_ptr()) };
        FolioGuard(self.0, PhantomData)
    }

    pub fn try_lock(&self) -> Option<FolioGuard<'_>> {
        unsafe { c::folio_trylock(self.0.as_ptr()) }.then(|| FolioGuard(self.0, PhantomData))
    }

    /// Locked: EAGAIN if @nonblock and that would wait.
    pub fn lock_or_eagain(&self, nonblock: bool) -> Result<FolioGuard<'_>, BchError> {
        if nonblock { self.try_lock().ok_or_else(|| c::EAGAIN.into()) } else { Ok(self.lock()) }
    }
}

/// A FolioRef's folio, locked: unlocked when dropped.
pub struct FolioGuard<'a>(Folio, PhantomData<&'a ()>);

impl FolioGuard<'_> {
    /// A folio handed to us locked - writeback_iter()'s - as its guard.
    ///
    /// # Safety
    /// @folio is locked, ours to unlock, and kept for 'a.
    pub unsafe fn adopt(folio: Folio) -> Self {
        FolioGuard(folio, PhantomData)
    }

    /// Left locked, for whoever we return it to: a fault's VM_FAULT_LOCKED.
    pub fn keep_locked(self) -> Folio {
        core::mem::ManuallyDrop::new(self).0
    }
}

impl Deref for FolioGuard<'_> {
    type Target = Folio;

    fn deref(&self) -> &Folio {
        &self.0
    }
}

impl Drop for FolioGuard<'_> {
    fn drop(&mut self) {
        unsafe { c::folio_unlock(self.0.as_ptr()) }
    }
}

pub struct LockedFolio(Folio);

impl LockedFolio {
    pub unsafe fn from_raw(folio: *mut c::folio) -> Self {
        LockedFolio(unsafe { Folio::from_raw(folio) })
    }

    /// Its lock and ref given up, to what holds them as C does.
    pub fn into_raw(self) -> *mut c::folio {
        core::mem::ManuallyDrop::new(self).0.as_ptr()
    }
}

impl Deref for LockedFolio {
    type Target = Folio;

    fn deref(&self) -> &Folio {
        &self.0
    }
}

impl Borrow<Folio> for LockedFolio {
    fn borrow(&self) -> &Folio {
        &self.0
    }
}

impl Drop for LockedFolio {
    fn drop(&mut self) {
        unsafe {
            c::folio_unlock(self.0.as_ptr());
            c::folio_put(self.0.as_ptr());
        }
    }
}

/// A folio for a mapping, Mapping::alloc_folio()'s, with a ref of its own:
/// not locked, and in no page cache till add_folio() puts it in, locked.
/// Dropped before then, its private is freed and it's put.
pub struct NewFolio {
    folio:    Folio,
    in_cache: bool,
}

impl NewFolio {
    /// In the page cache, which keeps it: its ref given up, and its lock the
    /// caller's.
    pub fn into_cached(self) -> Folio {
        assert!(self.in_cache, "a new folio given out before it's in the page cache");
        self.folio
    }
}

impl Deref for NewFolio {
    type Target = Folio;

    fn deref(&self) -> &Folio {
        &self.folio
    }
}

impl Drop for NewFolio {
    fn drop(&mut self) {
        if !self.in_cache {
            self.folio.free_private();
        }
        unsafe { c::folio_put(self.folio.as_ptr()) }
    }
}

/// Ordered by address: lock order, for two of them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mapping(NonNull<c::address_space>);

impl Mapping {
    /// # Safety
    /// @mapping outlives the Mapping.
    pub unsafe fn from_raw(mapping: *mut c::address_space) -> Self {
        Mapping(NonNull::new(mapping).expect("an address_space"))
    }

    pub fn as_ptr(self) -> *mut c::address_space {
        self.0.as_ptr()
    }

    pub fn is_empty(self) -> bool {
        unsafe { (*self.as_ptr()).nrpages == 0 }
    }

    /// @errno - a negative one - recorded for whoever next checks for a
    /// writeback error: mapping_set_error().
    pub fn set_error(self, errno: c_int) {
        unsafe { c::mapping_set_error(self.as_ptr(), errno) }
    }

    /// Its dirty pages from @start to @end, inclusive, written and waited on:
    /// filemap_write_and_wait_range(). A writeback error is the mapping's,
    /// whoever sees it first - a file's own is File::write_and_wait_range().
    pub fn write_and_wait_range(self, start: u64, end: u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::filemap_write_and_wait_range(self.as_ptr(), loff(start), loff(end)) })
    }

    /// Its folios from index @start to @end, inclusive, to @f in order until
    /// it breaks or errors: unlocked, for @f to lock or try to. A batch at a
    /// time, as filemap_get_folios() finds them, with a cond_resched() between.
    pub fn walk_folios<T, E>(self, start: u64, end: u64,
                             mut f: impl FnMut(FolioRef<'_>) -> Result<ControlFlow<T>, E>)
        -> Result<Option<T>, E>
    {
        let mut index = start as _;
        let mut batch = FolioBatch::new();
        while unsafe { c::filemap_get_folios(self.as_ptr(), &mut index, end as _, &mut batch.0) } != 0 {
            for &folio in batch.folios() {
                let folio = unsafe { FolioRef::from_raw(folio) };
                if let ControlFlow::Break(v) = f(folio)? {
                    return Ok(Some(v));
                }
            }
            batch.release();
            cond_resched();
        }
        Ok(None)
    }

    /// walk_folios(), for a walk that visits every folio: that can't fail,
    /// and doesn't stop early.
    pub fn for_each_folio(self, start: u64, end: u64, mut f: impl FnMut(FolioRef<'_>)) {
        let Ok(_) = self.walk_folios(start, end, |folio| {
            f(folio);
            Ok::<_, core::convert::Infallible>(ControlFlow::<()>::Continue(()))
        });
    }

    /// The folio at @index, if there is one: filemap_lock_folio().
    pub fn lock_folio(self, index: u64) -> Option<LockedFolio> {
        let folio = unsafe { c::filemap_lock_folio(self.as_ptr(), index as _) };
        // an ERR_PTR() for none, or NULL on older kernels
        NonNull::new(folio).and_then(|f| errptr_to_result(f).ok()).map(|f| LockedFolio(Folio(f)))
    }

    /// The folio at @index, made if there's none.
    pub fn lock_folio_create(self, index: u64, gfp: c::gfp_t) -> Result<LockedFolio, BchError> {
        self.get_folio(index, c::FGP_LOCK | c::FGP_CREAT, gfp)
    }

    /// The folio at @index, as __filemap_get_folio() finds it with @fgp -
    /// which has FGP_LOCK.
    pub fn get_folio(self, index: u64, fgp: c::fgf_t, gfp: c::gfp_t) -> Result<LockedFolio, BchError> {
        debug_assert!(fgp & c::FGP_LOCK != 0);
        self.get_folio_raw(index, fgp, gfp).map(|f| LockedFolio(Folio(f)))
    }

    /// The folio at @index, as __filemap_get_folio() finds it with @fgp: a
    /// ref, and locked if @fgp has FGP_LOCK - for C, which takes both.
    pub fn get_folio_raw(self, index: u64, fgp: c::fgf_t, gfp: c::gfp_t) -> Result<NonNull<c::folio>, BchError> {
        let gfp = crate::util::alloc::current_gfp_context_raw(gfp);
        let folio = unsafe { c::__filemap_get_folio(self.as_ptr(), index as _, fgp, gfp) };
        errptr_to_result(NonNull::new(folio).expect("a folio, or an error"))
    }

    /// Whether a userspace mapping of it can write: mapping_writably_mapped().
    pub fn writably_mapped(self) -> bool {
        unsafe { c::mapping_writably_mapped(self.as_ptr()) != 0 }
    }

    /// Throttled, if it's dirtied faster than writeback keeps up:
    /// balance_dirty_pages_ratelimited().
    pub fn balance_dirty_pages_ratelimited(self) {
        unsafe { c::balance_dirty_pages_ratelimited(self.as_ptr()) }
    }

    /// What its folios are allocated with: mapping_gfp_mask().
    pub fn gfp_mask(self) -> c::gfp_t {
        unsafe { c::mapping_gfp_mask(self.as_ptr()) }
    }

    /// @folio marked dirty: filemap_dirty_folio().
    pub fn dirty_folio(self, folio: Folio) -> bool {
        unsafe { c::filemap_dirty_folio(self.as_ptr(), folio.as_ptr()) }
    }

    pub fn readahead_gfp(self) -> c::gfp_t {
        unsafe { c::readahead_gfp_mask(self.as_ptr()) }
    }

    pub fn min_folio_order(self) -> u32 {
        unsafe { c::mapping_min_folio_order(self.as_ptr()) }
    }

    /// 0 on !THP, whatever was asked for at inode init: without THP a large
    /// folio in the page cache can never be split again.
    pub fn max_folio_order(self) -> u32 {
        unsafe { c::mapping_max_folio_order(self.as_ptr()) }
    }

    /// Whether a folio is in the page cache at @index - not a shadow entry.
    pub fn has_folio(self, index: u64) -> bool {
        unsafe {
            let e = c::xa_load(&raw mut (*self.as_ptr()).i_pages, index as _);
            !e.is_null() && !c::xa_is_value(e)
        }
    }

    /// A folio for this page cache, in none yet.
    pub fn alloc_folio(self, gfp: c::gfp_t, order: u32) -> Option<NewFolio> {
        let gfp = crate::util::alloc::current_gfp_context_raw(gfp);
        NonNull::new(unsafe { c::rust_filemap_alloc_folio(gfp, order) })
            .map(|f| NewFolio { folio: Folio(f), in_cache: false })
    }

    /// @folio at @index, locked: filemap_add_folio().
    pub fn add_folio(self, folio: &mut NewFolio, index: u64, gfp: c::gfp_t) -> Result<(), BchError> {
        assert!(!folio.in_cache, "a folio added to the page cache twice");
        let gfp = crate::util::alloc::current_gfp_context_raw(gfp);
        let ret = unsafe { c::filemap_add_folio(self.as_ptr(), folio.as_ptr(), index as _, gfp) };
        if ret < 0 {
            return Err(BchError::from_raw(ret));
        }
        folio.in_cache = true;
        Ok(())
    }
}

pub struct Readahead<'a>(&'a mut c::readahead_control);

impl Readahead<'_> {
    /// # Safety
    /// @ractl is the VFS's, for the call it's given to.
    pub unsafe fn from_raw(ractl: *mut c::readahead_control) -> Self {
        Readahead(unsafe { &mut *ractl })
    }

    pub fn mapping(&self) -> Mapping {
        unsafe { Mapping::from_raw(self.0.mapping) }
    }

    /// The next folio asked for, locked - the page cache keeps it:
    /// readahead_folio().
    pub fn next(&mut self) -> Option<Folio> {
        let folio = NonNull::new(unsafe { c::__readahead_folio(self.0) })?;
        unsafe { c::folio_put(folio.as_ptr()) };
        Some(Folio(folio))
    }

    /// @folio given back unread, its private freed, and with a ref for
    /// the VFS, which unlocks and puts it - readahead_folio() again. Back to
    /// front, so readahead_control's window stays contiguous.
    pub fn unread(&mut self, folio: Folio) {
        folio.free_private();
        unsafe { c::folio_get(folio.as_ptr()) };
        let n = folio.nr_pages();
        self.0._nr_pages += n as u32;
        self.0._index -= n as core::ffi::c_ulong;
    }
}

/// The VFS's writeback of a mapping, for the writepages() it's passed to.
pub(crate) struct Wbc(NonNull<c::writeback_control>);

from_raw! {
    Wbc = *mut c::writeback_control => |r| Wbc(NonNull::new_unchecked(r));
}

impl Wbc {
    /// The next folio to write back, locked - after @prev, which writing
    /// returned @err: writeback_iter().
    pub(crate) fn next(&self, mapping: Mapping, prev: Option<Folio>, err: &mut c_int) -> Option<FolioGuard<'_>> {
        let prev = prev.map_or(core::ptr::null_mut(), Folio::as_ptr);
        let folio = unsafe { c::writeback_iter(mapping.as_ptr(), self.0.as_ptr(), prev, err) };
        (!folio.is_null()).then(|| unsafe { FolioGuard::adopt(Folio::from_raw(folio)) })
    }

    /// wbc_to_write_flags(): what its writes are submitted as.
    pub(crate) fn write_flags(&self) -> c::blk_opf_t {
        unsafe { c::wbc_to_write_flags(self.0.as_ptr()) }
    }
}

/// write_begin() and write_end()'s first argument, which they don't use: the
/// kiocb from 6.17, the file before.
pub(crate) struct VfsWriter;

#[cfg(linux_ge = "6.17")]
from_raw! {
    VfsWriter = *const c::kiocb => |_r| VfsWriter;
}

#[cfg(not(linux_ge = "6.17"))]
from_raw! {
    VfsWriter = *mut c::file => |_r| VfsWriter;
}

from_raw! {
    Mapping               = *mut c::address_space => |r| Mapping::from_raw(r);
    Folio                 = *mut c::folio         => |r| Folio::from_raw(r);
    /// read_folio()'s: locked, its lock ours to give up, not its ref.
    FolioGuard<'a>        = *mut c::folio         => |r| FolioGuard::adopt(Folio::from_raw(r));
    Readahead<'a>         = *mut c::readahead_control => |r| Readahead::from_raw(r);
    /// write_end()'s: write_begin()'s, its lock and ref to give up.
    LockedFolio           = *mut c::folio         => |r| LockedFolio::from_raw(r);
    /// write_begin()'s out parameters.
    &'a mut *mut c::folio = *mut *mut c::folio    => |r| &mut *r;
    &'a mut *mut c_void   = *mut *mut c_void      => |r| &mut *r;
}
