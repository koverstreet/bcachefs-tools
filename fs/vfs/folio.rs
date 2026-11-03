// SPDX-License-Identifier: GPL-2.0

//! The page cache, as a read fills it.
//!
//! A Folio is a page cache folio the caller holds locked, as a read holds the
//! folios it fills: the lock keeps it in the page cache, so a Folio is Copy
//! and takes no ref; get() and put() are for the refs a caller has besides.
//! Its bcachefs state - bch_folio, vfs/pagecache.c's - is created on it before
//! it's read into, and partially_uptodate is the read's to keep; the rest is
//! under the state's lock, lock_state_if_any()'s.
//!
//! A LockedFolio is one looked up locked and with a ref, both its own: a
//! Folio until it's dropped, which unlocks and puts it. A FolioRef is one
//! not locked, kept by a ref someone else has - a walk's batch, walk_folios()'s,
//! or a fault's page - for its holder to lock: a FolioGuard, a Folio until
//! it's dropped.
//!
//! A Mapping is an inode's page cache: where a read finds and adds folios. A
//! Readahead is the VFS's readahead_control: the locked folios it asks a read
//! of, each given out with a ref, and given back - unread - by unread().

use super::inode::InodeInfo;
use crate::c;
use crate::errcode::{errptr_to_result, loff_to_result, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::locking::Guard;
use crate::util::kernel::{cond_resched, loff};
use core::ffi::{c_int, c_uint};
use core::marker::PhantomData;
use core::ops::{ControlFlow, Deref, DerefMut};
use core::ptr::NonNull;
use cstruct_macros::c_extern;

c_extern! {
    pub fn __bch2_folio_create(folio: *mut c::folio, gfp: c::gfp_t) -> *mut c::bch_folio;
    pub fn bch2_folio_create(folio: *mut c::folio, gfp: c::gfp_t) -> *mut c::bch_folio;
    fn bch2_seek_pagecache_data(vinode: *mut c::inode, start: c::loff_t, end: c::loff_t,
                                min_replicas: c_uint, nonblock: bool) -> c::loff_t;
    fn bch2_write_invalidate_inode_pages_range(mapping: *mut c::address_space, start: c::loff_t,
                                               end: c::loff_t) -> c_int;
    fn bch2_mark_pagecache_unallocated(inode: *mut c::bch_inode_info, start: u64, end: u64);
    fn bch2_seek_pagecache_hole(vinode: *mut c::inode, start: c::loff_t, end: c::loff_t,
                                min_replicas: c_uint, nonblock: bool) -> c::loff_t;
    fn bch2_clamp_data_hole(vinode: *mut c::inode, hole_start: *mut u64, hole_end: *mut u64,
                            min_replicas: c_uint, nonblock: bool) -> c_int;
    fn bch2_mark_pagecache_reserved(inode: *mut c::bch_inode_info, start: *mut u64, end: u64,
                                    nonblock: bool) -> c_int;
    fn bch2_folio_set(c: *mut Fs, inode: *mut c::bch_inode_info, folios: *mut *mut c::folio,
                      nr: c_uint) -> c_int;
    fn bch2_get_folio_disk_reservation(c: *mut Fs, inode: *mut c::bch_inode_info, folio: *mut c::folio,
                                       check_enospc: bool) -> c_int;
    // buffered.c's:
    fn bch2_read_single_folio(folio: *mut c::folio, mapping: *mut c::address_space, extend: bool) -> c_int;
}

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
        unsafe { c::rust_folio_size(self.as_ptr()) }
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
        unsafe { c::rust_folio_zero_segment(self.as_ptr(), start, end) }
    }

    /// Its writeable userspace mappings write-protected, so a write through
    /// one faults - page_mkwrite again: folio_mkclean().
    pub fn mkclean(self) {
        unsafe { c::rust_folio_mkclean(self.as_ptr()) };
    }

    /// Its data read in, @mapping's, before this returns:
    /// bch2_read_single_folio().
    pub fn read_single(self, mapping: Mapping) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_read_single_folio(self.as_ptr(), mapping.as_ptr(), false) })
    }

    /// Unlocked: the caller's lock, and with it the Folio, given up.
    pub fn end_read(self, uptodate: bool) {
        unsafe { c::folio_end_read(self.as_ptr(), uptodate) }
    }

    pub fn get(self) {
        unsafe { c::folio_get(self.as_ptr()) }
    }

    /// # Safety
    /// The caller has a ref, besides its lock, and gives it up.
    pub unsafe fn put(self) {
        unsafe { c::folio_put(self.as_ptr()) }
    }

    fn bch(self) -> Option<NonNull<c::bch_folio>> {
        NonNull::new(unsafe { c::rust_folio_get_private(self.as_ptr()) }.cast())
    }

    /// Its bcachefs state, made if it has none; false if that fails.
    pub fn bch_create(self, gfp: c::gfp_t) -> bool {
        !unsafe { bch2_folio_create(self.as_ptr(), gfp) }.is_null()
    }

    /// bch_create(), for a folio not yet in the page cache.
    pub fn bch_create_new(self, gfp: c::gfp_t) -> bool {
        !unsafe { __bch2_folio_create(self.as_ptr(), gfp) }.is_null()
    }

    /// Its bcachefs state freed: the folio's never been read into.
    pub fn bch_release(self) {
        unsafe { c::kfree(c::folio_detach_private(self.as_ptr())) }
    }

    /// Panics if it has no bcachefs state.
    pub fn partially_uptodate(self) -> u16 {
        unsafe { (*self.bch().expect("a folio's bch_folio").as_ptr()).partially_uptodate }
    }

    pub fn set_partially_uptodate(self, sectors: u16) {
        unsafe { (*self.bch().expect("a folio's bch_folio").as_ptr()).partially_uptodate = sectors }
    }

    /// Its bcachefs state, locked, if it has any.
    pub fn lock_state_if_any(&self) -> Option<FolioStateGuard<'_>> {
        let s = unsafe { self.bch()?.as_ref() };
        Some(FolioStateGuard { guard: s.state.lock(), sectors: self.nr_sectors() })
    }

    /// Its bcachefs state brought up to date with the btree: bch2_folio_set().
    pub fn bch_set(self, inode: &InodeInfo) -> Result<(), BchError> {
        let mut folio = self.as_ptr();
        ret_to_result_void(unsafe { bch2_folio_set(inode.fs().as_ptr(), inode.raw(), &mut folio, 1) })
    }

    /// The disk and quota reservation for writing all of it back:
    /// bch2_get_folio_disk_reservation().
    pub fn disk_reservation(self, inode: &InodeInfo, check_enospc: bool) -> Result<(), BchError> {
        ret_to_result_void(unsafe {
            bch2_get_folio_disk_reservation(inode.fs().as_ptr(), inode.raw(), self.as_ptr(), check_enospc)
        })
    }
}

/// A folio's bcachefs state, its lock held.
pub struct FolioStateGuard<'a> {
    guard:   Guard<'a, c::spinlock_t, c::FolioState>,
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

/// A walk's batch of folios, a ref on each: filemap_get_folios()'s, put when
/// it's released or dropped.
struct FolioBatch(c::folio_batch);

impl FolioBatch {
    fn new() -> Self {
        let mut b = FolioBatch(unsafe { core::mem::zeroed() });
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
}

/// A FolioRef's folio, locked: unlocked when dropped.
pub struct FolioGuard<'a>(Folio, PhantomData<&'a ()>);

impl FolioGuard<'_> {
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

impl Deref for LockedFolio {
    type Target = Folio;

    fn deref(&self) -> &Folio {
        &self.0
    }
}

impl Drop for LockedFolio {
    fn drop(&mut self) {
        unsafe {
            c::folio_unlock(self.0.as_ptr());
            self.0.put();
        }
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

    pub fn inode<'a>(self) -> &'a InodeInfo {
        unsafe { InodeInfo::from_inode((*self.as_ptr()).host) }
    }

    pub fn is_empty(self) -> bool {
        unsafe { (*self.as_ptr()).nrpages == 0 }
    }

    /// Its dirty pages from @start to @end, inclusive, written and waited on:
    /// filemap_write_and_wait_range(). A writeback error is the mapping's,
    /// whoever sees it first - a file's own is File::write_and_wait_range().
    pub fn write_and_wait_range(self, start: u64, end: u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::rust_filemap_write_and_wait_range(self.as_ptr(), loff(start), loff(end)) })
    }

    /// The first data in [@start, @end), or @end: dirty sectors of at least
    /// @min_replicas replicas, reserved ones counted. EAGAIN if @nonblock and
    /// it would have to wait on a folio lock: bch2_seek_pagecache_data().
    pub fn seek_data(self, start: u64, end: u64, min_replicas: u32, nonblock: bool) -> Result<u64, BchError> {
        loff_to_result(unsafe {
            bch2_seek_pagecache_data((*self.as_ptr()).host, loff(start), loff(end), min_replicas, nonblock)
        })
    }

    /// The first hole in [@start, @end), or @end - as seek_data() has data:
    /// bch2_seek_pagecache_hole().
    pub fn seek_hole(self, start: u64, end: u64, min_replicas: u32, nonblock: bool) -> Result<u64, BchError> {
        loff_to_result(unsafe {
            bch2_seek_pagecache_hole((*self.as_ptr()).host, loff(start), loff(end), min_replicas, nonblock)
        })
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

    /// Its pages from @start to @end, inclusive, written and dropped:
    /// bch2_write_invalidate_inode_pages_range().
    pub fn write_invalidate_range(self, start: u64, end: u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_write_invalidate_inode_pages_range(self.as_ptr(), loff(start), loff(end)) })
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
        let folio = unsafe { c::__filemap_get_folio(self.as_ptr(), index as _, fgp, gfp) };
        errptr_to_result(NonNull::new(folio).expect("a folio, or an error")).map(|f| LockedFolio(Folio(f)))
    }

    /// @folio marked dirty: filemap_dirty_folio().
    pub fn dirty_folio(self, folio: Folio) -> bool {
        unsafe { c::rust_filemap_dirty_folio(self.as_ptr(), folio.as_ptr()) }
    }

    /// The hole [@start, @end), in sectors, shrunk to where the page cache
    /// has no data: its start past the data at the front, its end at the next.
    /// EAGAIN if @nonblock and it would have to wait on a folio lock:
    /// bch2_clamp_data_hole().
    pub fn clamp_data_hole(self, start: &mut u64, end: &mut u64, min_replicas: u32, nonblock: bool)
        -> Result<(), BchError>
    {
        ret_to_result_void(unsafe { bch2_clamp_data_hole((*self.as_ptr()).host, start, end, min_replicas, nonblock) })
    }

    /// Its folios' sectors from @start to @end, in sectors, marked reserved,
    /// @start advanced past those done. EAGAIN if @nonblock and it would have
    /// to wait on a folio lock: bch2_mark_pagecache_reserved().
    pub fn mark_reserved(self, start: &mut u64, end: u64, nonblock: bool) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_mark_pagecache_reserved(self.inode().raw(), start, end, nonblock) })
    }

    /// Its folios' sectors from @start to @end, in sectors, marked
    /// unallocated: bch2_mark_pagecache_unallocated().
    pub fn mark_unallocated(self, start: u64, end: u64) {
        unsafe { bch2_mark_pagecache_unallocated(self.inode().raw(), start, end) }
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

    /// A folio for this page cache, locked and with a ref, in none yet.
    pub fn alloc_folio(self, gfp: c::gfp_t, order: u32) -> Option<Folio> {
        NonNull::new(unsafe { c::rust_filemap_alloc_folio(gfp, order) }).map(Folio)
    }

    /// @folio, alloc_folio()'s, at @index: locked, and the ref the caller
    /// keeps.
    pub fn add_folio(self, folio: Folio, index: u64, gfp: c::gfp_t) -> Result<(), BchError> {
        let ret = unsafe { c::filemap_add_folio(self.as_ptr(), folio.as_ptr(), index as _, gfp) };
        if ret < 0 { Err(BchError::from_raw(ret)) } else { Ok(()) }
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

    /// The next folio asked for: locked, and with a ref the caller puts.
    pub fn next(&mut self) -> Option<Folio> {
        NonNull::new(unsafe { c::__readahead_folio(self.0) }).map(Folio)
    }

    /// @folio given back unread, its bcachefs state freed: the VFS unlocks it.
    /// Back to front, so readahead_control's window stays contiguous.
    pub fn unread(&mut self, folio: Folio) {
        folio.bch_release();
        let n = folio.nr_pages();
        self.0._nr_pages += n as u32;
        self.0._index -= n as core::ffi::c_ulong;
    }
}
