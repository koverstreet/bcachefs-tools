// SPDX-License-Identifier: GPL-2.0

//! The page cache, as a read fills it and writeback empties it.
//!
//! A Folio is a page cache folio, held only by reference: &'a Folio is a
//! borrow of what holds it for 'a - its lock, or under writeback, its
//! writeback - and its private data is borrowed from that. What holds a lock
//! derefs to a FolioLocked, a Folio whose state is its holder's to change:
//!
//! - LockedFolio: looked up locked and with a ref, both its own; dropped, it
//!   unlocks and puts it.
//! - FolioGuard: its lock, the folio kept by a ref someone else has for 'a -
//!   a walk's batch - or by the lock itself, in a page cache: readahead's, a
//!   new folio's once it's in. Dropped, or end_read(), it unlocks; or
//!   keep_locked() gives the lock up, locked, to a bio or the VFS.
//! - HeldFolio: one the VFS has locked, lent to an op - which may free its
//!   private data.
//! - NewFolio: alloc_folio()'s, with a ref of its own, unlocked and in no page
//!   cache till add_folio() puts it in, locked.
//!
//! A FolioRef is one not locked, kept by a ref someone else has - a walk's
//! batch, or a fault's page - for its holder to lock: it has no Folio till
//! then.
//!
//! A Mapping is an inode's page cache: where a read finds and adds folios. A
//! Readahead is the VFS's readahead_control: the locked folios it asks a read
//! of, given back - unread - by unread().
//!
//! Each is typed by its mapping's operations, A - an inode's page cache by
//! the inode's, FsMapping::mapping()'s - and a folio's private data is
//! A::Private, the filesystem's, made and freed by its FolioPrivate. A is Any
//! where nothing of the filesystem's is touched.

use crate::c;
use crate::errcode::{errptr_to_result, ret_to_result_void, BchError};
use crate::os::file::Any;
use crate::os::iov_iter::IovIter;
use crate::os::kernel::{cond_resched, loff};
use crate::os::ops::from_raw;
use crate::os::super_block::IsInode;
use crate::util::alloc::KBox;
use crate::util::ffi::Opaque;
use core::borrow::Borrow;
use core::cell::Cell;
use core::cmp::Ordering;
use core::ffi::{c_int, c_uint, c_void};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{ControlFlow, Deref};
use core::ptr::NonNull;

/// A folio's private data: made for a folio of a size, and freed knowing it -
/// a folio's size doesn't change while it has private data, which is
/// released before a split.
pub trait FolioPrivate: Sized {
    /// One for a folio of @size bytes; None if it can't be allocated.
    fn alloc(size: usize) -> Option<NonNull<Self>>;

    /// # Safety
    /// @p is alloc(@size)'s, and nothing else has it.
    unsafe fn free(p: NonNull<Self>, size: usize);
}

/// An inode whose page cache is its own: it implements the address space
/// operations its mapping has.
///
/// # Safety
/// Every inode of Self's has as its i_mapping's a_ops a table of
/// AopsVTable<Self>'s.
pub unsafe trait FsMapping: IsInode + AddressSpaceOps {
    fn mapping(&self) -> Mapping<Self> {
        unsafe { Mapping::from_raw((*core::ptr::from_ref(self).cast::<c::inode>()).i_mapping) }
    }
}

#[repr(transparent)]
pub struct Folio<A = Any>(Opaque<c::folio>, PhantomData<fn() -> A>);

impl<A> Folio<A> {
    /// # Safety
    /// @folio is held for 'a - locked, or under writeback, its private data
    /// not freed - and is in, or for, a mapping of A's.
    pub unsafe fn from_raw<'a>(folio: NonNull<c::folio>) -> &'a Self {
        unsafe { folio.cast().as_ref() }
    }

    pub fn as_ptr(&self) -> *mut c::folio {
        self.0.as_ptr()
    }

    pub fn size(&self) -> usize {
        unsafe { c::folio_size(self.as_ptr()) }
    }

    pub fn nr_sectors(&self) -> usize {
        self.size() >> 9
    }

    pub fn nr_pages(&self) -> usize {
        unsafe { c::folio_nr_pages(self.as_ptr()) as usize }
    }

    pub fn pos(&self) -> u64 {
        unsafe { c::folio_pos(self.as_ptr()) as u64 }
    }

    pub fn end_pos(&self) -> u64 {
        self.pos() + self.size() as u64
    }

    pub fn sector(&self) -> u64 {
        self.pos() >> 9
    }

    pub fn end_sector(&self) -> u64 {
        self.sector() + (self.size() >> 9) as u64
    }

    pub fn uptodate(&self) -> bool {
        unsafe { c::folio_test_uptodate(self.as_ptr()) }
    }

    pub fn dirty(&self) -> bool {
        unsafe { c::folio_test_dirty(self.as_ptr()) }
    }

    pub fn writeback(&self) -> bool {
        unsafe { c::folio_test_writeback(self.as_ptr()) }
    }

    pub fn locked(&self) -> bool {
        unsafe { c::folio_test_locked(self.as_ptr()) }
    }

    /// Its mapping, unless it's been truncated out of it.
    pub fn mapping(&self) -> Option<Mapping<A>> {
        NonNull::new(unsafe { c::rust_folio_mapping(self.as_ptr()) }).map(Mapping::new)
    }

    /// Waited for, if its data is being written and mustn't change meanwhile:
    /// folio_wait_stable().
    pub fn wait_stable(&self) {
        unsafe { c::folio_wait_stable(self.as_ptr()) }
    }

    pub fn mark_uptodate(&self) {
        unsafe { c::folio_mark_uptodate(self.as_ptr()) }
    }

    /// Bytes [@start, @end) of it zeroed: folio_zero_segment().
    pub fn zero_segment(&self, start: usize, end: usize) {
        unsafe { c::folio_zero_segment(self.as_ptr(), start, end) }
    }

    /// Bytes [@start1, @end1) and [@start2, @end2) of it zeroed:
    /// folio_zero_segments().
    pub fn zero_segments(&self, start1: usize, end1: usize, start2: usize, end2: usize) {
        unsafe { c::folio_zero_segments(self.as_ptr(), start1, end1, start2, end2) }
    }

    /// @len bytes of it from @start zeroed: folio_zero_range().
    pub fn zero_range(&self, start: usize, len: usize) {
        unsafe { c::folio_zero_range(self.as_ptr(), start, len) }
    }

    /// What's written to it through the kernel's mapping made visible through
    /// userspace's: flush_dcache_folio().
    pub fn flush_dcache(&self) {
        unsafe { c::flush_dcache_folio(self.as_ptr()) }
    }

    /// Its writeable userspace mappings write-protected, so a write through
    /// one faults - page_mkwrite again: folio_mkclean().
    pub fn mkclean(&self) {
        unsafe { c::folio_mkclean(self.as_ptr()) };
    }
}

impl<A: AddressSpaceOps> Folio<A> {
    /// Its private data, if it has any.
    pub fn private(&self) -> Option<&A::Private> {
        let p = NonNull::new(unsafe { c::folio_get_private(self.as_ptr()) })?;
        Some(unsafe { p.cast::<A::Private>().as_ref() })
    }

    /// Its private data, if it has any, detached and freed.
    ///
    /// # Safety
    /// Nothing borrowed of its private data is used after: the caller holds
    /// it as what every borrow of it is of.
    unsafe fn free_private(&self) {
        if let Some(p) = NonNull::new(unsafe { c::folio_detach_private(self.as_ptr()) }) {
            unsafe { A::Private::free(p.cast(), self.size()) }
        }
    }
}

/// A Folio whose state is its holder's to change: locked - or in no page
/// cache yet, where nothing else can reach it.
#[repr(transparent)]
pub struct FolioLocked<A = Any>(Folio<A>);

impl<A> FolioLocked<A> {
    /// # Safety
    /// As Folio::from_raw(), and locked by the caller for 'a.
    unsafe fn from_raw<'a>(folio: NonNull<c::folio>) -> &'a Self {
        unsafe { folio.cast().as_ref() }
    }
}

impl<A: AddressSpaceOps> FolioLocked<A> {
    /// Its private data, made if it has none; None if that fails.
    pub fn create_private(&self) -> Option<&A::Private> {
        if let Some(p) = self.private() {
            return Some(p);
        }
        let p = A::Private::alloc(self.size())?;
        unsafe {
            c::folio_attach_private(self.as_ptr(), p.as_ptr().cast());
            Some(p.as_ref())
        }
    }
}

impl<A> Deref for FolioLocked<A> {
    type Target = Folio<A>;

    fn deref(&self) -> &Folio<A> {
        &self.0
    }
}

/// @len bytes of a folio, from @offset into it.
pub struct FolioSpan<'a, A = Any> {
    pub folio:  &'a Folio<A>,
    pub offset: usize,
    pub len:    usize,
}

// Not derived: that would bound A
impl<A> Clone for FolioSpan<'_, A> { fn clone(&self) -> Self { *self } }
impl<A> Copy for FolioSpan<'_, A> {}

impl<'a, A> FolioSpan<'a, A> {
    /// @folio's part of the file's [@start, @end).
    pub fn new(folio: &'a Folio<A>, start: u64, end: u64) -> Self {
        let start = start.max(folio.pos());
        FolioSpan {
            folio,
            offset: (start - folio.pos()) as usize,
            len:    (end.min(folio.end_pos()) - start) as usize,
        }
    }

    pub fn whole(folio: &'a Folio<A>) -> Self {
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
pub struct FolioRef<'r, A = Any>(NonNull<c::folio>, PhantomData<(&'r (), fn() -> A)>);

impl<A> FolioRef<'_, A> {
    /// # Safety
    /// @folio is kept, by a ref of the caller's or its caller's, for 'r, and
    /// in a mapping of A's.
    pub unsafe fn from_raw(folio: *mut c::folio) -> Self {
        FolioRef(NonNull::new(folio).expect("a folio"), PhantomData)
    }

    pub fn lock(&self) -> FolioGuard<'_, A> {
        unsafe { c::folio_lock(self.0.as_ptr()) };
        FolioGuard(self.0, PhantomData)
    }

    pub fn try_lock(&self) -> Option<FolioGuard<'_, A>> {
        unsafe { c::folio_trylock(self.0.as_ptr()) }.then(|| FolioGuard(self.0, PhantomData))
    }

    /// Locked: EAGAIN if @nonblock and that would wait.
    pub fn lock_or_eagain(&self, nonblock: bool) -> Result<FolioGuard<'_, A>, BchError> {
        if nonblock { self.try_lock().ok_or_else(|| c::EAGAIN.into()) } else { Ok(self.lock()) }
    }
}

/// A folio's lock, the folio kept for 'a: unlocked when dropped.
pub struct FolioGuard<'a, A = Any>(NonNull<c::folio>, PhantomData<(&'a (), fn() -> A)>);

impl<A> FolioGuard<'_, A> {
    /// A folio handed to us locked - writeback_iter()'s, or a bio's - as its
    /// guard.
    ///
    /// # Safety
    /// @folio is locked, ours to unlock, kept for 'a, and in a mapping of A's.
    pub unsafe fn adopt(folio: NonNull<c::folio>) -> Self {
        FolioGuard(folio, PhantomData)
    }

    /// Left locked, for whoever we give it to: a bio reading it, or a fault's
    /// VM_FAULT_LOCKED.
    pub fn keep_locked(self) -> NonNull<c::folio> {
        ManuallyDrop::new(self).0
    }

    /// Unlocked, its data read in - uptodate, if @uptodate: folio_end_read().
    pub fn end_read(self, uptodate: bool) {
        unsafe { c::folio_end_read(self.keep_locked().as_ptr(), uptodate) }
    }
}

impl<A> Deref for FolioGuard<'_, A> {
    type Target = FolioLocked<A>;

    fn deref(&self) -> &FolioLocked<A> {
        unsafe { FolioLocked::from_raw(self.0) }
    }
}

impl<A> Drop for FolioGuard<'_, A> {
    fn drop(&mut self) {
        unsafe { c::folio_unlock(self.0.as_ptr()) }
    }
}

/// A folio its caller has locked, lent to an op - which may free its private
/// data.
pub struct HeldFolio<'a, A>(&'a FolioLocked<A>);

impl<A: AddressSpaceOps> HeldFolio<'_, A> {
    /// Its private data, if it has any, detached and freed.
    pub fn free_private(self) {
        unsafe { self.0.free_private() }
    }
}

impl<A> Deref for HeldFolio<'_, A> {
    type Target = FolioLocked<A>;

    fn deref(&self) -> &FolioLocked<A> {
        self.0
    }
}

pub struct LockedFolio<A = Any>(NonNull<c::folio>, PhantomData<fn() -> A>);

impl<A> LockedFolio<A> {
    /// Its lock and ref given up, to what holds them as C does.
    pub fn into_raw(self) -> *mut c::folio {
        ManuallyDrop::new(self).0.as_ptr()
    }
}

impl<A> Deref for LockedFolio<A> {
    type Target = FolioLocked<A>;

    fn deref(&self) -> &FolioLocked<A> {
        unsafe { FolioLocked::from_raw(self.0) }
    }
}

impl<A> Borrow<FolioLocked<A>> for LockedFolio<A> {
    fn borrow(&self) -> &FolioLocked<A> {
        self
    }
}

impl<A> Drop for LockedFolio<A> {
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
pub struct NewFolio<A: AddressSpaceOps> {
    folio:    NonNull<c::folio>,
    in_cache: bool,
    _ops:     PhantomData<fn() -> A>,
}

impl<A: AddressSpaceOps> NewFolio<A> {
    /// In the page cache, which keeps it while it's locked: its ref given
    /// up, and its lock the guard's.
    pub fn into_cached(self) -> FolioGuard<'static, A> {
        assert!(self.in_cache, "a new folio given out before it's in the page cache");
        let folio = self.folio;
        drop(self);
        FolioGuard(folio, PhantomData)
    }
}

impl<A: AddressSpaceOps> Deref for NewFolio<A> {
    type Target = FolioLocked<A>;

    fn deref(&self) -> &FolioLocked<A> {
        unsafe { FolioLocked::from_raw(self.folio) }
    }
}

impl<A: AddressSpaceOps> Drop for NewFolio<A> {
    fn drop(&mut self) {
        if !self.in_cache {
            unsafe { self.free_private() }
        }
        unsafe { c::folio_put(self.folio.as_ptr()) }
    }
}

/// A page cache whose operations are A's - where A is an inode, that inode's.
/// Ordered by address: lock order, for two of them.
pub struct Mapping<A = Any>(NonNull<c::address_space>, PhantomData<fn() -> A>);

// Not derived: that would bound A
impl<A> Clone for Mapping<A> { fn clone(&self) -> Self { *self } }
impl<A> Copy for Mapping<A> {}
impl<A> PartialEq for Mapping<A> { fn eq(&self, other: &Self) -> bool { self.0 == other.0 } }
impl<A> Eq for Mapping<A> {}
impl<A> PartialOrd for Mapping<A> { fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) } }
impl<A> Ord for Mapping<A> { fn cmp(&self, other: &Self) -> Ordering { self.0.cmp(&other.0) } }

impl<A: IsInode> Mapping<A> {
    pub fn inode(&self) -> &A {
        unsafe { &*(*self.as_ptr()).host.cast::<A>() }
    }
}

impl<A> Mapping<A> {
    fn new(mapping: NonNull<c::address_space>) -> Self {
        Mapping(mapping, PhantomData)
    }

    /// # Safety
    /// @mapping outlives the Mapping, and its operations are A's.
    pub unsafe fn from_raw(mapping: *mut c::address_space) -> Self {
        Mapping::new(NonNull::new(mapping).expect("an address_space"))
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

    /// Writeback of its dirty pages started, not waited on: filemap_flush().
    pub fn flush(self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::filemap_flush(self.as_ptr()) })
    }

    /// Its folios from index @start to @end, inclusive, to @f in order until
    /// it breaks or errors: unlocked, for @f to lock or try to. A batch at a
    /// time, as filemap_get_folios() finds them, with a cond_resched() between.
    pub fn walk_folios<T, E>(self, start: u64, end: u64,
                             mut f: impl FnMut(FolioRef<'_, A>) -> Result<ControlFlow<T>, E>)
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
    pub fn for_each_folio(self, start: u64, end: u64, mut f: impl FnMut(FolioRef<'_, A>)) {
        let Ok(_) = self.walk_folios(start, end, |folio| {
            f(folio);
            Ok::<_, core::convert::Infallible>(ControlFlow::<()>::Continue(()))
        });
    }

    /// The folio at @index, if there is one: filemap_lock_folio().
    pub fn lock_folio(self, index: u64) -> Option<LockedFolio<A>> {
        let folio = unsafe { c::filemap_lock_folio(self.as_ptr(), index as _) };
        // an ERR_PTR() for none, or NULL on older kernels
        NonNull::new(folio).and_then(|f| errptr_to_result(f).ok()).map(|f| LockedFolio(f, PhantomData))
    }

    /// The folio at @index, as __filemap_get_folio() finds it with @fgp -
    /// which has FGP_LOCK.
    pub fn get_folio(self, index: u64, fgp: c::fgf_t, gfp: c::gfp_t) -> Result<LockedFolio<A>, BchError> {
        debug_assert!(fgp & c::FGP_LOCK != 0);
        let gfp = crate::util::alloc::current_gfp_context_raw(gfp);
        let folio = unsafe { c::__filemap_get_folio(self.as_ptr(), index as _, fgp, gfp) };
        errptr_to_result(NonNull::new(folio).expect("a folio, or an error")).map(|f| LockedFolio(f, PhantomData))
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
    pub fn dirty_folio(self, folio: &Folio<A>) -> bool {
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
}

impl<A: AddressSpaceOps> Mapping<A> {
    /// A folio for this page cache, in none yet.
    pub fn alloc_folio(self, gfp: c::gfp_t, order: u32) -> Option<NewFolio<A>> {
        let gfp = crate::util::alloc::current_gfp_context_raw(gfp);
        NonNull::new(unsafe { c::rust_filemap_alloc_folio(gfp, order) })
            .map(|folio| NewFolio { folio, in_cache: false, _ops: PhantomData })
    }

    /// @folio at @index, locked: filemap_add_folio().
    pub fn add_folio(self, folio: &mut NewFolio<A>, index: u64, gfp: c::gfp_t) -> Result<(), BchError> {
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

/// A readahead of a mapping of A's.
pub struct Readahead<'a, A = Any>(&'a mut c::readahead_control, PhantomData<fn() -> A>);

impl<A: AddressSpaceOps> Readahead<'_, A> {
    pub fn mapping(&self) -> Mapping<A> {
        unsafe { Mapping::from_raw(self.0.mapping) }
    }

    /// The next folio asked for, locked - the page cache keeps it:
    /// readahead_folio().
    pub fn next(&mut self) -> Option<FolioGuard<'static, A>> {
        let folio = NonNull::new(unsafe { c::__readahead_folio(self.0) })?;
        unsafe { c::folio_put(folio.as_ptr()) };
        Some(FolioGuard(folio, PhantomData))
    }

    /// @folio given back unread, its private freed, and locked and with a ref
    /// for the VFS, which unlocks and puts it - readahead_folio() again. Back
    /// to front, so readahead_control's window stays contiguous.
    pub fn unread(&mut self, folio: FolioGuard<'static, A>) {
        unsafe { folio.free_private() };
        let n = folio.nr_pages();
        let folio = folio.keep_locked();
        unsafe { c::folio_get(folio.as_ptr()) };
        self.0._nr_pages += n as u32;
        self.0._index -= n as core::ffi::c_ulong;
    }
}

/// The VFS's writeback of a mapping, for the writepages() it's passed to:
/// and the folio it last gave, for the next.
pub struct Wbc(NonNull<c::writeback_control>, Cell<*mut c::folio>);

from_raw! {
    Wbc = *mut c::writeback_control => |r| Wbc(NonNull::new_unchecked(r), Cell::new(core::ptr::null_mut()));
}

impl Wbc {
    /// The next folio to write back, locked - after the last, which writing
    /// returned @err: writeback_iter().
    pub(crate) fn next<A>(&self, mapping: Mapping<A>, err: &mut c_int) -> Option<FolioGuard<'_, A>> {
        let folio = unsafe { c::writeback_iter(mapping.as_ptr(), self.0.as_ptr(), self.1.get(), err) };
        self.1.set(folio);
        NonNull::new(folio).map(|f| unsafe { FolioGuard::adopt(f) })
    }

    /// wbc_to_write_flags(): what its writes are submitted as.
    pub(crate) fn write_flags(&self) -> c::blk_opf_t {
        unsafe { c::wbc_to_write_flags(self.0.as_ptr()) }
    }
}

/// write_begin() and write_end()'s first argument, which they don't use: the
/// kiocb from 6.17, the file before.
pub struct VfsWriter;

#[cfg(linux_ge = "6.17")]
from_raw! {
    VfsWriter = *const c::kiocb => |_r| VfsWriter;
}

#[cfg(not(linux_ge = "6.17"))]
from_raw! {
    VfsWriter = *mut c::file => |_r| VfsWriter;
}

// An op's mapping and folios, for a table of A's: A's
from_raw! {
    Mapping<A> [A: AddressSpaceOps]        = *mut c::address_space     => |r| Mapping::from_raw(r);
    /// dirty_folio()'s: for the call.
    &'a Folio<A> [A: AddressSpaceOps]      = *mut c::folio             => |r| Folio::from_raw(NonNull::new_unchecked(r));
    /// invalidate_folio()'s and release_folio()'s: locked by the VFS, for
    /// the call.
    HeldFolio<'a, A> [A: AddressSpaceOps]  = *mut c::folio             =>
        |r| HeldFolio(FolioLocked::from_raw(NonNull::new_unchecked(r)));
    /// read_folio()'s: locked, its lock ours to give up, not its ref.
    FolioGuard<'a, A> [A: AddressSpaceOps] = *mut c::folio             => |r| FolioGuard::adopt(NonNull::new_unchecked(r));
    Readahead<'a, A> [A: AddressSpaceOps]  = *mut c::readahead_control => |r| Readahead(&mut *r, PhantomData);
    /// write_end()'s: write_begin()'s, its lock and ref to give up.
    LockedFolio<A> [A: AddressSpaceOps]    = *mut c::folio             => |r| LockedFolio(NonNull::new_unchecked(r), PhantomData);
    /// write_begin()'s out parameters.
    &'a mut *mut c::folio                  = *mut *mut c::folio        => |r| &mut *r;
    &'a mut *mut c_void                    = *mut *mut c_void          => |r| &mut *r;
    /// write_end()'s fsdata: what write_begin() gave the VFS, given back.
    KBox<T> [T]                            = *mut c_void               => |r| KBox::from_raw(r.cast());
}

crate::vfs_ops! {
    /// A mapping's operations, address_space_operations: migration and memory
    /// errors the kernel's generic ones.
    pub trait AddressSpaceOps for c::address_space_operations, AopsVTable<T: AddressSpaceOps> = T, |ops| {
        #[cfg(CONFIG_MIGRATION)]
        {
            ops.migrate_folio = Some(c::filemap_migrate_folio);
        }
        ops.error_remove_folio = Some(c::generic_error_remove_folio);
    };
    // What write_begin() leaves write_end() of the write, in *@fsdata
    type FsData;
    // Its folios' private data
    type Private: FolioPrivate;

    /// @folio's data read in; its lock, the VFS's, given up either way.
    fn read_folio(_file: *mut c::file, folio: FolioGuard<'_, Self>) -> Result<(), BchError>;
    fn readahead(ra: Readahead<'_, Self>);
    fn writepages(mapping: Mapping<Self>, wbc: Wbc) -> c_int;
    /// @folio marked dirty: whether it wasn't.
    fn dirty_folio(mapping: Mapping<Self>, folio: &Folio<Self>) -> bool;
    /// The folio @len bytes at @pos are written into, locked, in *@foliop,
    /// and what write_end() has of the write, in *@fsdata.
    fn write_begin(_w: VfsWriter, mapping: Mapping<Self>, pos: c::loff_t, len: c_uint,
                   foliop: &mut *mut c::folio, fsdata: &mut *mut c_void) -> Result<(), BchError>;
    /// @copied of the @len bytes at @pos written into @folio, write_begin()'s:
    /// how many were.
    fn write_end(_w: VfsWriter, mapping: Mapping<Self>, pos: c::loff_t, len: c_uint, copied: c_uint,
                 folio: LockedFolio<Self>, begun: KBox<Self::FsData>) -> c_int;
    fn invalidate_folio(folio: HeldFolio<'_, Self>, offset: usize, len: usize);
    /// Whether @folio's filesystem state was freed, for the folio to be.
    fn release_folio(folio: HeldFolio<'_, Self>, gfp: c::gfp_t) -> bool;
}
