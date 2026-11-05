// SPDX-License-Identifier: GPL-2.0

//! Reserving for writing folios back, and dirtying them: a folio's sectors
//! each hold the disk reservation for the copies writeback will write, at
//! the count its bch_folio's replicas_reserved_at says, and a write's
//! FolioReservation the disk space and quota it hands them as it dirties
//! them.

use core::cmp::min;
use core::ffi::c_int;
use core::ops::Range;

use cstruct_macros::c_export;

use crate::alloc::capacity::DiskReservation;
use crate::c;
use crate::c::bch_folio_sector_state as SectorState;
use crate::c::bch_reservation_flags as ReservationFlags;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::types::RustOnly;
use crate::util::alloc::init_at;
use crate::util::ffi::Opaque;

use super::folio::{FolioStateGuard, Folio, Mapping};
use super::inode::{InodeInfo, QuotaReservation};
use super::pagecache::{blocks_in, blocks_out};

impl FolioStateGuard<'_> {
    /*
     * Raise the count the folio's reservation is held at, carrying what it holds
     * across. The inode's count can change under dirty folios, so whatever charges
     * next has to land in the slot the rest is already in.
     */
    fn raise_reserved_at(&mut self, fs: &Fs, nr_replicas: u32) {
        let at = u32::from(self.replicas_reserved_at);
        if nr_replicas <= at {
            return;
        }

        let reserved = self.sectors().iter().map(|s| u64::from(s.bits.replicas_reserved())).sum();
        fs.reserved_move_slot(reserved, at, nr_replicas);
        self.replicas_reserved_at = nr_replicas as u8;
    }

    fn sectors_short(&mut self, sectors: Range<usize>, nr_replicas: u32) -> bool {
        self.sectors()[sectors].iter().any(|s| s.replicas_to_reserve(nr_replicas) != 0)
    }

    fn sectors_unallocated(&mut self, sectors: Range<usize>) -> u64 {
        self.sectors()[sectors].iter().filter(|s| s.state() == SectorState::SECTOR_unallocated).count() as u64
    }
}

impl Folio {
    /*
     * Every sector at the inode's count, not just the copies each lacks: writeback
     * rewrites every copy, and a reservation's count is the placement question it
     * asks. What sectors already hold is over-reserved, and goes back when
     * @disk_res is put.
     *
     * This can't fail: it's the backstop for overwrite credit lost since the
     * folio was dirtied (snapshots, reflink, EC, compression). But if a
     * foreground reservation already fell back, that was a decision - retry the
     * normal way, which falls back too, and only insist if that fails.
     */
    pub fn reserve_writeback(self, inode: &InodeInfo) -> Result<(), BchError> {
        let fs = inode.fs();
        let nr_replicas = inode.data_replicas();
        let sectors = self.nr_sectors();

        let degraded = {
            let mut s = self.lock_state();
            debug_assert!(s.state_uptodate);

            if !s.sectors_short(0..sectors, nr_replicas) {
                return Ok(());
            }
            s.reserved_degraded
        };

        let nofail = ReservationFlags::BCH_DISK_RESERVATION_NOFAIL;
        let disk_res = DiskReservation::new(fs);
        disk_res.add(sectors as u64, nr_replicas, if degraded { ReservationFlags::empty() } else { nofail })
            .or_else(|_| disk_res.add(sectors as u64, nr_replicas, nofail))?;

        /* what we can place, before it's raised to the folio's slot below: */
        let granted = disk_res.nr_replicas.get();

        /*
         * Only what each sector lacks: reserved copies stay until the sector
         * is written, so topping up clean sectors would pile them up.
         */
        let mut s = self.lock_state();
        s.raise_reserved_at(fs, granted);
        disk_res.set_nr_replicas(s.replicas_reserved_at.into());

        for x in s.sectors() {
            let r = x.replicas_to_reserve(granted);

            x.bits.set_replicas_reserved(x.bits.replicas_reserved() + r as u8);
            disk_res.sectors.set(disk_res.sectors.get() - u64::from(r));
        }

        Ok(())
    }
}

/// How FolioReservation::reserve() gets its disk space: all of it or
/// nothing, as much as there is, or all of it regardless - the quota then
/// not checked either.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reserve {
    Whole,
    Partial,
    Nofail,
}

/// A write's reservation for the folios it dirties: disk space for its
/// sectors at the inode's replicas, and quota for those it allocates - put
/// when it's dropped. C has its size, for buffered.c's to hold.
pub struct FolioReservation<'i> {
    disk:     DiskReservation<'i>,
    quota:    QuotaReservation<'i>,
    /* @disk fell back to fewer replicas than the inode asks for */
    degraded: bool,
}

impl<'i> FolioReservation<'i> {
    pub fn new(inode: &'i InodeInfo) -> Self {
        let disk = DiskReservation::new(inode.fs());
        disk.nr_replicas.set(inode.data_replicas());
        FolioReservation { disk, quota: inode.quota_reservation(), degraded: false }
    }

    /*
     * If the reservation fell back, @res drops to the count we got: the folio is
     * charged at @res's count, and the rest of this write shouldn't ask again.
     */
    fn reserve(&mut self, folio: Folio, offset: usize, len: usize, mode: Reserve) -> Result<usize, BchError> {
        let fs = self.disk.c;
        let mut sectors = blocks_out(fs, offset, len);
        let mut reserved = len;
        let nr_replicas = self.disk.nr_replicas.get();

        let disk_res = DiskReservation::new(fs);

        let short = {
            let mut s = folio.lock_state();
            assert!(s.state_uptodate);
            s.sectors_short(sectors.clone(), nr_replicas)
        };

        if short {
            disk_res.add(sectors.len() as u64, nr_replicas, match mode {
                Reserve::Whole   => ReservationFlags::empty(),
                Reserve::Partial => ReservationFlags::BCH_DISK_RESERVATION_PARTIAL,
                Reserve::Nofail  => ReservationFlags::BCH_DISK_RESERVATION_NOFAIL,
            })?;

            let got = (disk_res.sectors.get() / u64::from(disk_res.nr_replicas.get())) as usize;
            if got < sectors.len() {
                /*
                 * Make sure to get a reservation that's aligned to the
                 * filesystem blocksize:
                 */
                let block_bytes = fs.block_bytes() as usize;
                let reserved_offset = ((sectors.start + got) << 9) / block_bytes * block_bytes;
                reserved = reserved_offset.clamp(offset, offset + len) - offset;

                if reserved == 0 {
                    return fs.throw(bch_errcode::BCH_ERR_ENOSPC_disk_reservation);
                }

                sectors.end = blocks_out(fs, offset, reserved).end;
            }

            if disk_res.nr_replicas.get() < nr_replicas {
                self.disk.move_slot(disk_res.nr_replicas.get());
                self.degraded = true;
            }
        }

        let quota_sectors = folio.lock_state().sectors_unallocated(sectors);
        if quota_sectors != 0 {
            /* FIXME: for Nofail, we'll need to make sure this won't fail with -ENOMEM */
            self.quota.add(quota_sectors, mode != Reserve::Nofail)?;
        }

        self.disk.merge(disk_res);
        Ok(reserved)
    }

    pub fn add(&mut self, folio: Folio, offset: usize, len: usize) -> Result<(), BchError> {
        self.reserve(folio, offset, len, Reserve::Whole).map(drop)
    }

    /// As much of [@offset, @offset + @len) as there's space for, in whole
    /// blocks: how much.
    pub fn add_partial(&mut self, folio: Folio, offset: usize, len: usize) -> Result<usize, BchError> {
        self.reserve(folio, offset, len, Reserve::Partial)
    }

    fn add_nofail(&mut self, folio: Folio, offset: usize, len: usize) -> Result<(), BchError> {
        self.reserve(folio, offset, len, Reserve::Nofail).map(drop)
    }

    /// The reservation's sectors moved into @folio's, [@offset, @offset + @len)
    /// of it dirtied: whether the folio wasn't dirty before.
    pub fn dirty(&mut self, folio: Folio, offset: usize, len: usize) -> bool {
        let inode = self.quota.inode();
        let fs = inode.fs();
        let mut dirty_sectors = 0;

        /*
         * We'll get called by the mm code when propagating the dirty bit from
         * the PTE to the folio, and address_space_operations.dirty_folio
         * doesn't know the difference between a page and a folio.
         *
         * Potentially a nasty performance bug, we'll be dirtying more than
         * we're supposed to.
         *
         * We may also legitimately be dirtying folios above i_size: truncate_setsize()
         * drops i_size before truncate_pagecache() runs, and folios pinned by
         * gup (e.g. DIO read destinations into an mmap'd region of this file)
         * stay attached to the address_space until the pin drops. The VFS
         * truncate path documents this in truncate_cleanup_folio() and handles
         * it via folio_cancel_dirty() ordering; __bch2_writepage cleans up the
         * sectors_dirty bits we set here.
         */

        let sectors = blocks_out(fs, offset, len);
        debug_assert!(sectors.end <= u32::MAX as usize);

        let nr_replicas = self.disk.nr_replicas.get();

        {
            let mut s = folio.lock_state();
            assert!(s.state_uptodate);

            /* both have to name the same slot: raise whichever is lower */
            s.raise_reserved_at(fs, nr_replicas);
            self.disk.set_nr_replicas(s.replicas_reserved_at.into());

            if self.degraded {
                s.reserved_degraded = true;
            }

            for x in &mut s.sectors()[sectors] {
                /*
                 * This can happen if we race with the error path in
                 * bch2_writepage_io_done():
                 */
                let replicas = min(u64::from(x.replicas_to_reserve(nr_replicas)), self.disk.sectors.get());

                x.bits.set_replicas_reserved(x.bits.replicas_reserved() + replicas as u8);
                self.disk.sectors.set(self.disk.sectors.get() - replicas);

                dirty_sectors += i64::from(x.state() == SectorState::SECTOR_unallocated);

                x.set_state(x.state().dirty());
            }
        }

        inode.i_sectors_acct(Some(&mut *self.quota), dirty_sectors);

        if !folio.dirty() { inode.vfs().mapping().dirty_folio(folio) } else { false }
    }
}

#[c_export]
pub fn bch2_folio_reservation_init(_c: &Opaque<Fs>, inode: &InodeInfo, res: *mut c::bch2_folio_reservation) {
    // the inode outlives the write C holds the reservation for
    let r = unsafe { core::mem::transmute::<FolioReservation<'_>, FolioReservation<'static>>(FolioReservation::new(inode)) };
    let Ok(()) = unsafe { init_at(RustOnly::new(r), &raw mut (*res).r) };
}

#[c_export]
pub fn bch2_folio_reservation_put(_c: &Opaque<Fs>, _inode: &InodeInfo, res: *mut c::bch2_folio_reservation) {
    unsafe { core::ptr::drop_in_place(&raw mut (*res).r) }
}

#[c_export]
pub fn bch2_folio_reservation_get(_c: &Opaque<Fs>, _inode: &InodeInfo, folio: *mut c::folio,
                                  res: &mut c::bch2_folio_reservation, offset: usize, len: usize) -> c_int {
    ret_to_c(res.r.add(unsafe { Folio::from_raw(folio) }, offset, len))
}

#[c_export]
pub fn bch2_folio_reservation_get_partial(_c: &Opaque<Fs>, _inode: &InodeInfo, folio: *mut c::folio,
                                          res: &mut c::bch2_folio_reservation, offset: usize, len: usize) -> isize {
    match res.r.add_partial(unsafe { Folio::from_raw(folio) }, offset, len) {
        Ok(reserved) => reserved as isize,
        Err(e) => ret_to_c(Err::<(), _>(e)) as isize,
    }
}

impl InodeInfo {
    pub fn undirty_folio(&self, folio: Folio, offset: usize, len: usize) {
        let fs = self.fs();
        let disk_res = DiskReservation::new(fs);
        let mut dirty_sectors = 0;

        {
            let Some(mut s) = folio.lock_state_if_any() else { return };

            /* release at the count the folio's sectors were charged at */
            disk_res.nr_replicas.set(s.replicas_reserved_at.into());

            for x in s.sectors().get_mut(blocks_in(fs, offset, len)).into_iter().flatten() {
                disk_res.sectors.set(disk_res.sectors.get() + u64::from(x.bits.replicas_reserved()));
                x.bits.set_replicas_reserved(0);

                dirty_sectors -= i64::from(x.state() == SectorState::SECTOR_dirty);
                x.set_state(x.state().undirty());
            }
        }

        self.i_sectors_acct(None, dirty_sectors);
    }

}

#[c_export]
pub fn bch2_set_folio_dirty(_c: &Opaque<Fs>, _inode: &InodeInfo, folio: *mut c::folio,
                            res: &mut c::bch2_folio_reservation, offset: usize, len: usize) -> bool {
    res.r.dirty(unsafe { Folio::from_raw(folio) }, offset, len)
}

#[c_export]
pub fn bch2_vfs_dirty_folio(mapping: *mut c::address_space, folio: *mut c::folio) -> bool {
    let inode = unsafe { Mapping::from_raw(mapping) }.inode();
    let folio = unsafe { Folio::from_raw(folio) };

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
    res.add_nofail(folio, 0, dirty_bytes).expect("a nofail folio reservation");
    res.dirty(folio, 0, dirty_bytes)
}

