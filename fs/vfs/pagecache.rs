// SPDX-License-Identifier: GPL-2.0

//! bcachefs's state for a folio of the page cache - each sector's allocation
//! state and replicas - and what reads and sets it: made, set from the btree
//! and from a read, marked by walks over the page cache, read by its seeks,
//! and freed by the aops. Reserving and dirtying are folio_reservation.rs's.

use core::cmp::{max, min};
use core::ffi::c_int;
use core::mem::size_of;
use core::ops::{ControlFlow, Range};
use core::ptr::NonNull;

use cstruct_macros::{c_export, c_extern};
use kernel::alloc::flags::{GFP_KERNEL, __GFP_ZERO};
use kernel::alloc::Flags;
use kernel::page::PAGE_SHIFT;

use crate::btree::bkey::{pos, BkeySC};
use crate::btree::iter::{BtreeIter, BtreeIterFlags};
use crate::c;
use crate::c::bch_folio_sector_state as SectorState;
use crate::data::durability::BkeyVDurability;
use crate::data::extents::BkeyVExtents;
use crate::errcode::{errptr_to_result, ret_to_c, BchError};
use crate::format::Btree;
use crate::fs::Fs;
use crate::new_lock;
use crate::snapshots::subvolume;
use crate::util::alloc::{current_gfp_context_raw, gfp_flags, init_at, kmalloc};
use crate::util::kernel::PAGE_SECTORS_SHIFT;
use crate::util::bio::Bio;
use crate::util::ffi::Opaque;

use super::folio::{Folio, FolioStateGuard, Mapping};
use super::inode::InodeInfo;

c_extern! {
    fn __bch2_darray_resize_noprof(d: *mut c::darray_char, element_size: usize, new_size: usize,
                                   gfp: c::gfp_t, rcu: bool, have_prealloc: bool) -> c_int;
}

#[c_export]
pub fn bch2_filemap_get_contig_folios_d(mapping: *mut c::address_space, start: c::loff_t, end: u64,
                                        mut fgp_flags: c::fgf_t, gfp: c::gfp_t, fs: &mut c::folios) -> c_int {
    let start = start as u64;
    let mut pos = start;
    let mut ret = 0;

    while pos < end {
        if pos >= start + (1 << 20) {
            fgp_flags &= !c::FGP_CREAT;
        }

        if fs.nr + 1 > fs.size {
            ret = unsafe {
                __bch2_darray_resize_noprof((&raw mut *fs).cast(), size_of::<*mut c::folio>(), fs.nr + 1,
                                            current_gfp_context_raw(gfp & c::GFP_KERNEL), false, false)
            };
            if ret != 0 {
                break;
            }
        }

        let mapping = unsafe { Mapping::from_raw(mapping) };
        let Ok(f) = mapping.get_folio_raw(pos >> PAGE_SHIFT, fgp_flags, gfp) else { break };
        let folio = unsafe { Folio::from_raw(f.as_ptr()) };

        assert!(fs.nr == 0 || folio.pos() == pos);

        pos = folio.end_pos();
        unsafe { fs.data.add(fs.nr).write(f.as_ptr()) };
        fs.nr += 1;
    }

    if fs.nr == 0 && ret == 0 && fgp_flags & c::FGP_CREAT != 0 {
        ret = -(c::ENOMEM as c_int);
    }

    if fs.nr != 0 { 0 } else { ret }
}

#[c_export]
pub fn bch2_write_invalidate_inode_pages_range(mapping: *mut c::address_space, start: c::loff_t,
                                               end: c::loff_t) -> c_int {
    ret_to_c(unsafe { Mapping::from_raw(mapping) }.write_invalidate_range(start as u64, end as u64))
}

impl c::bch_folio_sector {
    pub(crate) fn state(&self) -> SectorState {
        SectorState::from_bits(self.state.into())
    }

    pub(crate) fn set_state(&mut self, state: SectorState) {
        self.state = state as u8;
    }

    /// Whether it has data: dirty, or written.
    pub(crate) fn has_data(&self) -> bool {
        self.state >= SectorState::SECTOR_dirty as u8
    }

    pub(super) fn replicas(&self) -> u32 {
        u32::from(self.bits.nr_replicas()) + u32::from(self.bits.replicas_reserved())
    }

    pub(super) fn replicas_to_reserve(&self, nr_replicas: u32) -> u32 {
        nr_replicas.saturating_sub(self.replicas())
    }
}

impl Folio {
    /* for newly allocated folios: */
    fn alloc_state(self, gfp: Flags) -> Option<NonNull<c::bch_folio>> {
        let size = size_of::<c::bch_folio>() + size_of::<c::bch_folio_sector>() * self.nr_sectors();
        let s = kmalloc(size, gfp | __GFP_ZERO).ok()?.cast::<c::bch_folio>();
        let state = c::FolioState { state_uptodate: false, replicas_reserved_at: 0, reserved_degraded: false, s: [] };
        let Ok(()) = unsafe { init_at(new_lock!(state), &raw mut (*s.as_ptr()).state) };
        unsafe { c::folio_attach_private(self.as_ptr(), s.as_ptr().cast()) };
        Some(s)
    }

    /// Its bcachefs state, made if it has none; false if that fails.
    pub fn create_state(self, gfp: Flags) -> bool {
        debug_assert!(self.locked());

        self.state_ptr().is_some() || self.alloc_state(gfp).is_some()
    }

    /// Its bcachefs state brought up to date with the btree.
    pub fn update_state(self, inode: &InodeInfo) -> Result<(), BchError> {
        inode.read_folio_state(&[self])
    }

    /* file offset (to folio offset) to bch_folio_sector index */
    fn sector_index(self, pos: u64) -> usize {
        assert!(pos >= self.pos() && pos < self.end_pos());
        ((pos - self.pos()) >> 9) as usize
    }

    /// The sectors of [@start, @end), in sectors, that are this folio's, as
    /// indices of its sectors.
    fn sector_range(self, start: u64, end: u64) -> Range<usize> {
        let (folio_start, folio_end) = (self.sector(), self.end_sector());
        let (start, end) = (max(start, folio_start), min(end, folio_end));

        assert!(start <= end, "sectors {start}..{end} outside folio {folio_start}..{folio_end}");
        (start - folio_start) as usize..(end - folio_start) as usize
    }
}

/// The sectors of [@offset, @offset + @len), in bytes, rounded out to whole
/// blocks.
pub(super) fn blocks_out(fs: &Fs, offset: usize, len: usize) -> Range<usize> {
    let block_bytes = fs.block_bytes() as usize;
    offset / block_bytes * block_bytes >> 9..(offset + len).next_multiple_of(block_bytes) >> 9
}

/// As blocks_out(), rounded in: only the whole blocks.
pub(super) fn blocks_in(fs: &Fs, offset: usize, len: usize) -> Range<usize> {
    let block_bytes = fs.block_bytes() as usize;
    offset.next_multiple_of(block_bytes) >> 9..(offset + len) / block_bytes * block_bytes >> 9
}

#[c_export]
pub fn __bch2_folio_create(folio: *mut c::folio, gfp: c::gfp_t) -> *mut c::bch_folio {
    unsafe { Folio::from_raw(folio) }.alloc_state(gfp_flags(gfp)).map_or(core::ptr::null_mut(), NonNull::as_ptr)
}

#[c_export]
pub fn bch2_folio_create(folio: *mut c::folio, gfp: c::gfp_t) -> *mut c::bch_folio {
    let folio = unsafe { Folio::from_raw(folio) };
    folio.create_state(gfp_flags(gfp));
    folio.state_ptr().map_or(core::ptr::null_mut(), NonNull::as_ptr)
}

impl SectorState {
    fn of_key(fs: &Fs, k: BkeySC<'_>) -> Self {
        if k.extent_is_reservation(fs) {
            SectorState::SECTOR_reserved
        } else if k.k.extent_is_allocation() {
            SectorState::SECTOR_allocated
        } else {
            SectorState::SECTOR_unallocated
        }
    }
}

impl InodeInfo {
    /* XXX: this should not be open coded */
    pub fn data_replicas(&self) -> u32 {
        match self.ei_inode.bi_data_replicas.get() {
            0 => u32::from(self.fs().opts.data_replicas.get()),
            r => u32::from(r) - 1,
        }
    }

    /*
     * Initialize bch_folio state (allocated/unallocated, nr_replicas) from the
     * extents btree:
     */
    pub fn read_folio_state(&self, folios: &[Folio]) -> Result<(), BchError> {
        let fs = self.fs();
        let offset = folios[0].sector();
        let mut need_set = false;

        for &f in folios {
            if !f.create_state(GFP_KERNEL) {
                return Err(c::ENOMEM.into());
            }

            if f.pos()     >= self.ei_reserved.ei_reserved_start.get() &&
               f.end_pos() <= self.ei_reserved.ei_reserved_end.get() &&
               !f.lock_state().state_uptodate {
                let mut r = self.ei_reserved.lock();
                let (reserved, range) = r.parts();

                if f.pos()     >= range.ei_reserved_start.get() &&
                   f.end_pos() <= range.ei_reserved_end.get() {
                    f.lock_state().set_sectors(0..f.nr_sectors(), reserved.ei_reserved_replicas.into(),
                                          SectorState::from_bits(reserved.ei_reserved_state.into()));
                    range.ei_reserved_start.set(f.end_pos());
                }
            }

            need_set |= !f.lock_state().state_uptodate;
        }

        if !need_set {
            return Ok(());
        }

        /* The extents past the folios, while they're alike: what InodeReserved caches */
        struct Run {
            end:      u64,
            replicas: u32,
            state:    SectorState,
            keys:     u32,
        }

        let inum = self.inum();
        let mut unset = folios.iter().copied().peekable();
        let mut run: Option<Run> = None;

        let flags = BtreeIterFlags::SLOTS | BtreeIterFlags::TRIM;
        let trans = crate::btree_trans!(fs);
        let mut iter = BtreeIter::new(&trans, Btree::extents, pos(inum.inum, offset), flags);
        subvolume::for_each_in_subvolume_max(&trans, &mut iter, pos(inum.inum, u64::MAX), inum.subvol as u32, flags,
                                             |_, _, k| {
            let nr_ptrs = u32::from(k.durability_safe(fs).nr_overwritable);
            let state = SectorState::of_key(fs, k);

            while let Some(&folio) = unset.peek() {
                let sectors = folio.sector_range(k.k.start_offset(), k.k.p.offset);

                let mut s = folio.lock_state();
                if !s.state_uptodate {
                    s.set_sectors(sectors, nr_ptrs, state);
                }

                if k.k.p.offset < folio.end_sector() {
                    break;
                }
                unset.next();
            }

            if unset.peek().is_none() {
                let r = run.get_or_insert(Run { end: 0, replicas: nr_ptrs, state, keys: 0 });
                if r.replicas != nr_ptrs || r.state != state || r.keys > 16 {
                    return Ok(ControlFlow::Break(()));
                }

                r.end = k.k.p.offset << 9;
                r.keys += 1;
            }
            Ok(ControlFlow::Continue(()))
        })?;

        /* Build up in locals, publish once: see InodeReserved */
        if let Some(run) = run {
            let mut r = self.ei_reserved.lock();
            let (reserved, range) = r.parts();
            range.ei_reserved_start.set(folios.last().expect("folios to set").end_pos());
            range.ei_reserved_end.set(run.end);
            reserved.ei_reserved_replicas = run.replicas as u8;
            reserved.ei_reserved_state = run.state as u8;
        }

        Ok(())
    }
}

#[c_export]
pub fn bch2_folio_set(_c: &Opaque<Fs>, inode: &InodeInfo, folios: *mut *mut c::folio, nr: u32) -> c_int {
    let folios = unsafe { core::slice::from_raw_parts(folios.cast::<Folio>(), nr as usize) };
    ret_to_c(inode.read_folio_state(folios))
}

#[c_export]
pub fn bch2_bio_page_state_set(c: &Opaque<Fs>, bio: *mut c::bio, k: BkeySC<'_>) {
    let fs = Fs::from_c(c);
    let nr_ptrs = if k.k.key_type() == c::bch_bkey_type::KEY_TYPE_reflink_v {
        0
    } else {
        u32::from(k.durability_safe(fs).nr_overwritable)
    };
    let state = SectorState::of_key(fs, k);

    for fv in unsafe { Bio::from_raw(bio) }.range().folios() {
        unsafe { Folio::from_raw(fv.folio.as_ptr()) }.lock_state()
            .set_sectors(fv.offset >> 9..(fv.offset >> 9) + (fv.len >> 9), nr_ptrs, state);
    }
}

impl Mapping {
    /// Its folios' sectors from @start to @end, in sectors, marked
    /// unallocated.
    pub fn mark_unallocated(self, start: u64, end: u64) {
        if end <= start {
            return;
        }

        self.for_each_folio(start >> PAGE_SECTORS_SHIFT, (end - 1) >> PAGE_SECTORS_SHIFT, |f| {
            let folio = f.lock();
            let sectors = folio.sector_range(start, end);

            if let Some(mut s) = folio.lock_state_if_any() {
                for x in &mut s.sectors()[sectors] {
                    x.bits.set_nr_replicas(0);
                }
            }
        });
    }

    /// Its folios' sectors from @start to @end, in sectors, marked reserved,
    /// @start advanced past those done. EAGAIN if @nonblock and it would have
    /// to wait on a folio lock.
    pub fn mark_reserved(self, start: &mut u64, end: u64, nonblock: bool) -> Result<(), BchError> {
        if end <= *start {
            return Ok(());
        }

        let inode = self.inode();
        let mut i_sectors_delta = 0;

        let ret = self.walk_folios(*start >> PAGE_SECTORS_SHIFT, (end - 1) >> PAGE_SECTORS_SHIFT, |f| {
            let folio = f.lock_or_eagain(nonblock)?;

            if let Some(mut s) = folio.lock_state_if_any() {
                for x in &mut s.sectors()[folio.sector_range(*start, end)] {
                    i_sectors_delta -= i64::from(x.state() == SectorState::SECTOR_dirty);
                    x.set_state(x.state().reserve());
                }
            }

            *start = min(end, folio.end_sector());
            Ok(ControlFlow::<()>::Continue(()))
        });

        inode.i_sectors_acct(None, i_sectors_delta);
        ret.map(|_| ())
    }
}

impl FolioStateGuard<'_> {
    fn set_sectors(&mut self, sectors: Range<usize>, nr_ptrs: u32, state: SectorState) {
        assert!(sectors.start < self.sectors().len());
        let uptodate = sectors.end == self.sectors().len();

        for x in &mut self.sectors()[sectors] {
            x.bits.set_nr_replicas(nr_ptrs as u8);
            x.set_state(state);
        }

        if uptodate {
            self.state_uptodate = true;
        }
    }
}

impl Folio {
    fn release_state(self) {
        let inode = self.mapping().expect("a folio in the page cache").inode();

        inode.undirty_folio(self, 0, self.size());

        debug_assert!(self.locked());
        self.free_state();
    }
}

#[c_export]
pub fn bch2_invalidate_folio(folio: *mut c::folio, offset: usize, length: usize) {
    let folio = unsafe { Folio::from_raw(folio) };

    if offset != 0 || length < folio.size() {
        return;
    }

    folio.release_state();
}

#[c_export]
pub fn bch2_release_folio(folio: *mut c::folio, _gfp_mask: c::gfp_t) -> bool {
    let folio = unsafe { Folio::from_raw(folio) };

    if folio.dirty() || folio.writeback() {
        return false;
    }

    folio.release_state();
    true
}

/* fseek: */

impl Folio {
    fn data_offset(self, pos: u64, min_replicas: u32) -> Option<u64> {
        let mut s = self.lock_state_if_any()?;
        let start = self.sector_index(pos);

        s.sectors()[start..].iter()
            .position(|x| x.has_data() && x.replicas() >= min_replicas)
            .map(|i| ((start + i) << 9) as u64)
    }
}

impl Mapping {
    /// The first data in [@start, @end), or @end: dirty sectors of at least
    /// @min_replicas replicas, reserved ones counted. EAGAIN if @nonblock and
    /// it would have to wait on a folio lock.
    pub fn seek_data(self, start: u64, end: u64, min_replicas: u32, nonblock: bool) -> Result<u64, BchError> {
        let found = self.walk_folios(start >> PAGE_SHIFT, end >> PAGE_SHIFT, |f| {
            let folio = f.lock_or_eagain(nonblock)?;

            Ok::<_, BchError>(match folio.data_offset(max(folio.pos(), start), min_replicas) {
                Some(offset) => ControlFlow::Break(min(max(folio.pos() + offset, start), end)),
                None         => ControlFlow::Continue(()),
            })
        })?;

        Ok(found.unwrap_or(end))
    }

    /*
     * Search for a hole in a folio: whether there's one at @offset - none if
     * the folio is filled with data, @offset moved past it. A folio not in the
     * page cache is a hole. EAGAIN if @nonblock and it would wait.
     */
    fn folio_hole(self, offset: &mut u64, min_replicas: u32, nonblock: bool) -> Result<bool, BchError> {
        let fgp = c::FGP_LOCK | if nonblock { c::FGP_NOWAIT } else { 0 };
        let folio = match self.get_folio(*offset >> PAGE_SHIFT, fgp, 0) {
            Ok(f) => f,
            Err(e) if e.matches(c::ENOENT) => return Ok(true),
            Err(e) => return Err(e),
        };

        let Some(mut s) = folio.lock_state_if_any() else { return Ok(true) };
        let start = folio.sector_index(*offset);

        match s.sectors()[start..].iter().position(|x| !x.has_data() || x.replicas() < min_replicas) {
            Some(i) => {
                *offset = max(*offset, folio.pos() + ((start + i) << 9) as u64);
                Ok(true)
            }
            None => {
                *offset = folio.end_pos();
                Ok(false)
            }
        }
    }

    /// The first hole in [@start, @end), or @end - as seek_data() has data.
    pub fn seek_hole(self, start: u64, end: u64, min_replicas: u32, nonblock: bool) -> Result<u64, BchError> {
        let mut offset = start;

        while offset < end && !self.folio_hole(&mut offset, min_replicas, nonblock)? {}

        Ok(min(offset, end))
    }

    /// The hole [@start, @end), in sectors, shrunk to where the page cache
    /// has no data: its start past the data at the front, its end at the next.
    /// EAGAIN if @nonblock and it would have to wait on a folio lock.
    pub fn clamp_data_hole(self, start: &mut u64, end: &mut u64, min_replicas: u32, nonblock: bool)
        -> Result<(), BchError>
    {
        *start = self.seek_hole(*start << 9, *end << 9, min_replicas, nonblock)? >> 9;

        if *start == *end {
            return Ok(());
        }

        *end = self.seek_data(*start << 9, *end << 9, min_replicas, nonblock)? >> 9;
        Ok(())
    }
}
