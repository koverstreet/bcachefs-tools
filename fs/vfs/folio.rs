// SPDX-License-Identifier: GPL-2.0

//! The page cache, as a read fills it.
//!
//! A Folio is a page cache folio the caller holds locked, as a read holds the
//! folios it fills: the lock keeps it in the page cache, so a Folio is Copy
//! and takes no ref; get() and put() are for the refs a caller has besides.
//! Its bcachefs state - bch_folio, vfs/pagecache.c's - is created on it before
//! it's read into, and partially_uptodate is the read's to keep.
//!
//! A Mapping is an inode's page cache: where a read finds and adds folios. A
//! Readahead is the VFS's readahead_control: the locked folios it asks a read
//! of, each given out with a ref, and given back - unread - by unread().

use super::inode::InodeInfo;
use crate::c;
use crate::errcode::BchError;
use core::ptr::NonNull;
use cstruct_macros::c_extern;

c_extern! {
    pub fn __bch2_folio_create(folio: *mut c::folio, gfp: c::gfp_t) -> *mut c::bch_folio;
    pub fn bch2_folio_create(folio: *mut c::folio, gfp: c::gfp_t) -> *mut c::bch_folio;
}

#[derive(Clone, Copy, PartialEq, Eq)]
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

    pub fn nr_pages(self) -> usize {
        unsafe { c::folio_nr_pages(self.as_ptr()) as usize }
    }

    pub fn sector(self) -> u64 {
        unsafe { c::folio_pos(self.as_ptr()) as u64 >> 9 }
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

    pub fn mark_uptodate(self) {
        unsafe { c::folio_mark_uptodate(self.as_ptr()) }
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
}

#[derive(Clone, Copy)]
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
