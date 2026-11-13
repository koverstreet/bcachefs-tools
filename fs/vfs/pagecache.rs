// SPDX-License-Identifier: GPL-2.0

//! bcachefs's state for a folio of the page cache - each sector's allocation
//! state and replicas - and what reads and sets it: made, set from the btree
//! and from a read, marked by walks over the page cache, read by its seeks,
//! and freed by the aops. Reserving and dirtying are folio_reservation.rs's.

use core::cmp::{max, min};
use core::alloc::Layout;
use core::mem::{align_of, size_of};
use core::borrow::Borrow;
use core::ffi::{c_int, c_uint, c_void};
use core::ops::{ControlFlow, Range};
use core::ptr::NonNull;

use cstruct_macros::vtable;
use kernel::page::PAGE_SHIFT;

use crate::btree::bkey::{pos, BkeySC};
use crate::btree::iter::{BtreeIter, BtreeIterFlags};
use crate::c;
use crate::c::FolioSectorState as SectorState;
use crate::data::durability::BkeyVDurability;
use crate::data::extents::BkeyVExtents;
use crate::errcode::{errptr_to_result, BchError};
use crate::format::{Btree, BkeyType};
use crate::fs::Fs;
use crate::new_lock;
use crate::snapshots::subvolume;
use crate::locking::MemallocFlags;
use crate::data_structures::inline_vec::InlineVec;
use crate::types::CStatic;
use crate::util::alloc::{init_at, kzalloc, KBox};
use crate::os::kernel::PAGE_SECTORS_SHIFT;
use crate::util::bio::BioRange;

use super::buffered;
use super::buffered_write::{self, WriteBegun};
use super::folio::{AddressSpaceOps, AopsVTable, Folio, FolioGuard, FolioSpan, FolioStateGuard, LockedFolio, Mapping,
                   Readahead};
use super::folio_reservation::FolioReservation;
use super::inode::InodeInfo;
use super::ops::VfsWriter;
use super::writeback::{self, Wbc};

impl Mapping {
    /// The folios over [@start, @end), contiguous from @start, as
    /// __filemap_get_folio() finds them with @fgp_flags - made, with FGP_CREAT,
    /// only in the first megabyte: at least one, or why there's none.
    pub fn get_contig_folios(self, start: u64, end: u64, mut fgp_flags: c::fgf_t, gfp: c::gfp_t)
        -> Result<ContigFolios, BchError>
    {
        let mut fs = InlineVec::new();
        let mut pos = start;
        let mut ret: Result<(), BchError> = Ok(());

        while pos < end {
            if pos >= start + (1 << 20) {
                fgp_flags &= !c::FGP_CREAT;
            }

            if let Err(e) = fs.reserve(1) {
                ret = Err(e.into());
                break;
            }

            let Ok(folio) = self.get_folio(pos >> PAGE_SHIFT, fgp_flags, gfp) else { break };

            assert!(fs.is_empty() || folio.pos() == pos);

            pos = folio.end_pos();
            fs.push(folio).expect("a folio, room reserved for it");
        }

        if fs.is_empty() {
            ret?;
            if fgp_flags & c::FGP_CREAT != 0 {
                return Err(BchError::from_raw(c::ENOMEM as i32));
            }
        }
        Ok(ContigFolios { folios: fs, pos: start, end: end.min(pos) })
    }
}

/// Locked folios, each starting where the one before ends, over [pos, end):
/// only the first starts before pos, only the last ends past end - and none,
/// once pos == end.
pub struct ContigFolios {
    folios: InlineVec<LockedFolio, 8>,
    pos:    u64,
    end:    u64,
}

impl ContigFolios {
    pub fn pos(&self) -> u64 {
        self.pos
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    pub fn folios(&self) -> &[LockedFolio] {
        &self.folios
    }

    pub fn first(&self) -> Folio {
        *self.folios[0]
    }

    pub fn last(&self) -> Folio {
        *self.folios[self.folios.len() - 1]
    }

    /// The first folio, if [pos, end) starts partway into it.
    pub fn head(&self) -> Option<Folio> {
        self.folios.first().map(|f| **f).filter(|f| f.pos() != self.pos)
    }

    /// The last folio, if [pos, end) ends partway into it.
    pub fn tail(&self) -> Option<Folio> {
        self.folios.last().map(|f| **f).filter(|f| f.end_pos() != self.end)
    }

    /// Each folio's part of [pos, end).
    pub fn spans(&self) -> impl Iterator<Item = FolioSpan> + '_ {
        self.folios.iter().map(|f| FolioSpan::new(**f, self.pos, self.end))
    }

    pub fn flush_dcache(&self) {
        for f in self.folios.iter() {
            f.flush_dcache();
        }
    }

    /// Cut to end at @end, the folios past it dropped.
    pub fn truncate(&mut self, end: u64) {
        debug_assert!(end >= self.pos);
        self.end = self.end.min(end);
        let nr = self.folios.partition_point(|f| f.pos() < self.end);
        self.folios.truncate(nr);
    }
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
        self.state >= SectorState::dirty as u8
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
    pub(super) fn alloc_state(self) -> Option<NonNull<c::bch_folio>> {
        let size = size_of::<c::bch_folio>() + size_of::<c::bch_folio_sector>() * self.nr_sectors();
        let layout = Layout::from_size_align(size, align_of::<c::bch_folio>()).ok()?;
        let s = kzalloc(layout).ok()?.cast::<c::bch_folio>();
        let state = c::FolioState { state_uptodate: false, replicas_reserved_at: 0, reserved_degraded: false, s: [] };
        let Ok(()) = unsafe { init_at(new_lock!(state), &raw mut (*s.as_ptr()).state) };
        unsafe { c::folio_attach_private(self.as_ptr(), s.as_ptr().cast()) };
        Some(s)
    }

    /// Its bcachefs state, made if it has none; false if that fails.
    pub fn create_state(self) -> bool {
        debug_assert!(self.locked());

        self.state_ptr().is_some() || self.alloc_state().is_some()
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

impl SectorState {
    fn of_key(fs: &Fs, k: BkeySC<'_>) -> Self {
        if k.extent_is_reservation(fs) {
            SectorState::reserved
        } else if k.k.extent_is_allocation() {
            SectorState::allocated
        } else {
            SectorState::unallocated
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
    pub fn read_folio_state<F: Borrow<Folio>>(&self, folios: &[F]) -> Result<(), BchError> {
        let fs = self.fs();
        let folio = |f: &F| -> Folio { *f.borrow() };
        let offset = folio(&folios[0]).sector();
        let mut need_set = false;

        for f in folios.iter().map(folio) {
            if !f.create_state() {
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
        let mut unset = folios.iter().map(folio).peekable();
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
            range.ei_reserved_start.set(folio(folios.last().expect("folios to set")).end_pos());
            range.ei_reserved_end.set(run.end);
            reserved.ei_reserved_replicas = run.replicas as u8;
            reserved.ei_reserved_state = run.state as u8;
        }

        Ok(())
    }
}

/// The sectors of @range's folios, set as @k has them.
pub fn bio_page_state_set(fs: &Fs, range: BioRange<'_>, k: BkeySC<'_>) {
    let nr_ptrs = if k.k.key_type() == BkeyType::reflink_v {
        0
    } else {
        u32::from(k.durability_safe(fs).nr_overwritable)
    };
    let state = SectorState::of_key(fs, k);

    for fv in range.folios() {
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
                    i_sectors_delta -= i64::from(x.state() == SectorState::dirty);
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
        let mapping = self.mapping().expect("a folio in the page cache");
        let inode = mapping.inode();

        inode.undirty_folio(FolioSpan::whole(self));

        debug_assert!(self.locked());
        self.free_state();
    }
}

#[vtable]
impl AddressSpaceOps for InodeInfo {
    fn read_folio(_file: *mut c::file, folio: FolioGuard<'_>) -> Result<(), BchError> {
        let mapping = folio.mapping().expect("a folio to read, in its mapping");
        folio.read_single(mapping, false)
    }

    fn readahead(ra: Readahead<'_>) {
        buffered::readahead(ra)
    }

    fn writepages(mapping: Mapping, wbc: Wbc) -> c_int {
        writeback::writepages(mapping, wbc)
    }

    fn dirty_folio(mapping: Mapping, folio: Folio) -> bool {
        let inode = mapping.inode();

        /*
         * Folios above i_size are expected here rather than a curiosity:
         * truncate_setsize() drops i_size before truncate_pagecache() runs, so
         * there's a window where the mapping still holds folios past the new
         * end, and a folio pinned by gup stays attached to the address_space
         * until the pin drops. FolioReservation::dirty() has the longer version.
         *
         * A folio entirely past i_size has nothing to reserve or account for, so
         * drop it; one straddling i_size is clamped by the min() below and only
         * the part still within the file gets dirtied.
         */
        let file_size = inode.vfs().size();
        if folio.pos() >= file_size {
            return false;
        }

        let dirty_bytes = (min(folio.end_pos(), file_size) - folio.pos()).next_multiple_of(inode.fs().block_bytes()) as usize;

        let mut res = FolioReservation::new(inode);
        let span = FolioSpan { folio, offset: 0, len: dirty_bytes };
        res.add_nofail(span).expect("a nofail folio reservation");
        res.dirty(span)
    }

    fn write_begin(_w: VfsWriter, mapping: Mapping, pos: c::loff_t, len: c_uint,
                   foliop: &mut *mut c::folio, fsdata: &mut *mut c_void) -> Result<(), BchError> {
        *fsdata = core::ptr::null_mut();
        // 'static for WriteBegun's guards: the VFS holds the inode from here
        // to write_end(), which they're given back to
        let inode = unsafe { &*core::ptr::from_ref(mapping.inode()) };
        let (folio, begun) = buffered_write::write_begin(inode, mapping, pos as u64, len as usize)?;
        *foliop = folio.into_raw();
        *fsdata = KBox::into_raw(begun).cast();
        Ok(())
    }

    fn write_end(_w: VfsWriter, mapping: Mapping, pos: c::loff_t, len: c_uint, copied: c_uint,
                 folio: LockedFolio, begun: KBox<WriteBegun>) -> c_int {
        buffered_write::write_end(mapping.inode(), folio, begun, pos as u64, len as usize, copied as usize)
            as c_int
    }

    fn invalidate_folio(folio: Folio, offset: usize, len: usize) {
        if offset == 0 && len >= folio.size() {
            folio.release_state();
        }
    }

    fn release_folio(folio: Folio, _gfp: c::gfp_t) -> bool {
        if folio.dirty() || folio.writeback() {
            return false;
        }

        folio.release_state();
        true
    }
}

/// An inode's mapping's operations, as NewInode::init() sets them.
pub(crate) static INODE_AOPS: CStatic<c::address_space_operations> = unsafe {
    CStatic::new({
        let mut ops = AopsVTable::<InodeInfo>::OPS;
        #[cfg(CONFIG_MIGRATION)]
        {
            ops.migrate_folio  = Some(c::filemap_migrate_folio);
        }
        ops.error_remove_folio = Some(c::generic_error_remove_folio);
        ops
    })
};

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
