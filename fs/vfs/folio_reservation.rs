// SPDX-License-Identifier: GPL-2.0

//! Reserving for writing folios back, and dirtying them: a folio's sectors
//! each hold the disk reservation for the copies writeback will write, at
//! the count its bch_folio's replicas_reserved_at says, and a write's
//! FolioReservation the disk space and quota it hands them as it dirties
//! them.

use core::cmp::min;
use core::ops::Range;

use crate::alloc::capacity::DiskReservation;
use crate::c;
use crate::c::FolioSectorState as SectorState;
use crate::c::ReservationFlags as ReservationFlags;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;

use super::folio::{FolioSpan, FolioStateGuard, Folio};
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
        self.sectors()[sectors].iter().filter(|s| s.state() == SectorState::unallocated).count() as u64
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
/// when it's dropped.
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
    fn reserve(&mut self, span: FolioSpan, mode: Reserve) -> Result<usize, BchError> {
        let FolioSpan { folio, offset, len } = span;
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
                    return fs.throw(Errcode::BCH_ERR_ENOSPC_disk_reservation);
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

    pub fn add(&mut self, span: FolioSpan) -> Result<(), BchError> {
        self.reserve(span, Reserve::Whole).map(drop)
    }

    /// As much of @span as there's space for, in whole blocks: how much.
    pub fn add_partial(&mut self, span: FolioSpan) -> Result<usize, BchError> {
        self.reserve(span, Reserve::Partial)
    }

    pub(super) fn add_nofail(&mut self, span: FolioSpan) -> Result<(), BchError> {
        self.reserve(span, Reserve::Nofail).map(drop)
    }

    /// The reservation's sectors moved into @span's folio, and @span dirtied:
    /// whether the folio wasn't dirty before.
    pub fn dirty(&mut self, span: FolioSpan) -> bool {
        let FolioSpan { folio, offset, len } = span;
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

                dirty_sectors += i64::from(x.state() == SectorState::unallocated);

                x.set_state(x.state().dirty());
            }
        }

        inode.i_sectors_acct(Some(&mut *self.quota), dirty_sectors);

        if !folio.dirty() { inode.vfs().mapping().dirty_folio(folio) } else { false }
    }
}

impl InodeInfo {
    pub fn undirty_folio(&self, span: FolioSpan) {
        let FolioSpan { folio, offset, len } = span;
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

                dirty_sectors -= i64::from(x.state() == SectorState::dirty);
                x.set_state(x.state().undirty());
            }
        }

        self.i_sectors_acct(None, dirty_sectors);
    }

}
