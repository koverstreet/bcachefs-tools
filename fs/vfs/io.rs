// SPDX-License-Identifier: GPL-2.0

use core::ffi::c_int;
use core::ops::ControlFlow;
use core::pin::Pin;
use kernel::page::PAGE_SHIFT;

use pin_init::{pin_data, pin_init, stack_pin_init};

use crate::btree::bkey::{bkey_ge, pos};
use crate::btree::iter::{lockrestart_do, trans_commit_do, BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt};
use crate::c;
use crate::data::durability::BkeyVDurability;
use crate::data::extents::BkeyVExtents;
use crate::data::{io_misc, reflink};
use crate::errcode::{Errcode, ret_to_class, BchError};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, id};
use crate::{inode, inode_opts};
use crate::debug::trace::events;
use crate::sb::members::DevIoRef;
use crate::snapshots::subvolume;
use crate::util::bio::{Bio, Gathered};
use crate::os::file::File;
use crate::util::gather::Gather;
use crate::os::kernel::{current_id, ktime_get_coarse_real_ts64, ktime_get_ns, PAGE_BYTES, PAGE_SECTORS, PAGE_SECTORS_SHIFT};
use crate::util::log::LogMsg;
use crate::util::refcount::TryRefcount;
use crate::util::rcu::read_lock;
use crate::util::task::waiter;
use crate::util::Nsecs;
use crate::{fsck_err_on, has_bio, prt};

use super::iattr::Iattr;
use super::idmap::MntIdmap;
use super::inode::{InodeInfo, QuotaReservation, ATTR_CTIME, ATTR_MTIME, ATTR_SIZE};
use super::vfs_inode::VfsInode;

#[pin_data]
#[repr(C)]
pub(crate) struct NocowFlush {
    ca:   DevIoRef<c::DevWriteRefKind>,
    #[pin]
    bio:  Bio,
}
has_bio!(NocowFlush, bio);

impl Gathered<()> for NocowFlush {
    fn done(self: Pin<&mut Self>, _: Result<(), BchError>) {}
}

impl InodeInfo {
    fn flush_nocow_writes(&self) {
        let fs = self.fs();

        /*
         * Fetch-and-clear must be atomic: bch2_write_endio() sets bits here
         * from bio completion, with no serialization against us. A plain
         * copy + memset can erase a bit set in between - fsync would then
         * complete without flushing a device that had a completed write.
         * Bits set after the exchange belong to the next flush.
         *
         * The plain read first keeps the common nothing-to-flush case off the
         * atomic path: a bit that shows up after it is no different from one
         * that shows up after the exchange, and is picked up by the next flush
         * either way.
         */
        let mut devs = c::bch_devs_mask::default();
        self.ei_devs_need_flush.take(&mut devs.d);

        stack_pin_init!(let g = Gather::<(), waiter::Sync>::new());
        for dev in devs.d.iter() {
            let ca = fs.dev_rcu(dev as u32, &read_lock()).and_then(|ca| fs.io_ref_tryget::<c::DevWriteRefKind>(ca));
            let Some(ca) = ca else { continue };
            let bdev = ca.disk_sb.bdev;

            let bio = fs.vfs.nocow_flush_bioset
                .alloc::<BchError, _>(bdev, 0, c::req_op_REQ_OP_WRITE | c::REQ_PREFLUSH, |b| pin_init!(NocowFlush {
                    ca,
                    bio <- b.init::<BchError>(),
                }? BchError))
                .expect("a nocow flush bio: the bioset's mempool waits rather than fail");
            bio.submit_gathered(g.get());
        }
        g.wait_sync();
    }

    /*
     * inode->ei_inode.bi_journal_seq won't be up to date since it's set in an
     * insert trigger: look up the btree inode instead
     */
    fn flush(&self, flushed_seq: &mut u64) -> Result<(), BchError> {
        let fs = self.fs();
        *flushed_seq = 0;

        if fs.opts.journal_flush_disabled.get() != 0 {
            return Ok(());
        }

        let Some(_write) = fs.writes.tryget_live() else {
            return Err(c::EROFS.into());
        };

        trans_commit_do(fs, None, CommitFlags::empty(), |t| {
            *flushed_seq = inode_journal_seq(t, self.inum())?;
            Ok(())
        })?;
        fs.journal.flush_seq(*flushed_seq, c::TASK_INTERRUPTIBLE)?;
        self.flush_nocow_writes();
        Ok(())
    }
}

fn inode_journal_seq(t: &TransAttempt<'_, '_>, inum: c::subvol_inum) -> Result<u64, BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::uninit();
    let mut u = inode::__peek(t, &mut iter, inum, BtreeIterFlags::empty(),
                             Some(crate::here!()))?;

    let cur_seq = fs.journal_cur_seq();
    let seq = cur_seq.min(u.bi_journal_seq);

    if fsck_err_on!(t, u.bi_journal_seq > cur_seq, id::inode_journal_seq_in_future,
                    "inode journal seq in future (currently at {cur_seq})\n{u}")? {
        u.bi_journal_seq = cur_seq;
        inode::write(t, &mut iter, &mut u)?;
    }
    Ok(seq)
}

pub(super) fn fsync(inode: &InodeInfo, file: &File, start: c::loff_t, end: c::loff_t, datasync: c_int)
    -> Result<(), BchError>
{
    let fs = inode.fs();
    let start_time = ktime_get_ns();
    let mut flushed_seq = 0;

    let ret = file.write_and_wait_range(start as u64, end as u64)
        .and_then(|()| inode.vfs().sync_metadata())
        .and_then(|()| inode.flush(&mut flushed_seq))
        .map_err(|e| if e.matches(c::EROFS) { c::EIO.into() } else { e });

    let wb_err = file.check_and_advance_wb_err();
    let ret = ret.and(wb_err);

    fs.event_inc_trace(events::fsync, |out| {
        prt!(out, "inum: {}\n", inode.inum().inum);
        prt!(out, "subvol: {}\n", inode.inum().subvol);
        prt!(out, "flushed_seq: {flushed_seq}\n");
        prt!(out, "datasync: {datasync}\n");
        prt!(out, "journal_flush_disabled: {}\n", fs.opts.journal_flush_disabled.get());
        prt!(out, "ret: {}\n", ret_to_class(ret));
        prt!(out, "duration: {}\n", Nsecs(ktime_get_ns() - start_time));
    });

    ret
}

/* truncate: */

const MAX_LFS_FILESIZE: u64 = c::MAX_LFS_FILESIZE as u64;

fn range_has_data(fs: &Fs, subvol: u32, start: Bpos, end: Bpos) -> Result<bool, BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, Btree::extents, start, BtreeIterFlags::TRIM);
    subvolume::for_each_in_subvolume_max(&trans, &mut iter, end, subvol, BtreeIterFlags::TRIM,
                                                  |_, _, k| {
        Ok(if k.k.extent_is_data() && !k.extent_is_unwritten(fs) {
            ControlFlow::Break(true)
        } else {
            ControlFlow::Continue(())
        })
    })
}

impl InodeInfo {
    pub fn zero_pagecache_posteof(&self) {
        let v = self.vfs();
        let i_size = v.size();
        v.truncate_pagecache(i_size);

        if i_size % PAGE_BYTES == 0 {
            return;
        }

        if let Some(f) = v.mapping().lock_folio(i_size >> PAGE_SHIFT) {
            f.zero_segment((i_size - f.pos()) as usize, f.size());
        }
    }

    /// Whether writeback writes the folio back, and with it i_size: if not,
    /// the caller updates i_size.
    fn truncate_folio(&self, index: u64, start: u64, end: u64) -> Result<bool, BchError> {
        use c::FolioSectorState as SectorState;

        let fs = self.fs();
        let mapping = self.vfs().mapping();
        let inum = self.inum();

        let folio = match mapping.lock_folio(index) {
            Some(folio) => folio,
            None => {
                /*
                 * XXX: we're doing two index lookups when we end up reading the
                 * folio
                 */
                let sector = index << PAGE_SECTORS_SHIFT;
                if !range_has_data(fs, inum.subvol as u32, pos(inum.inum, sector),
                                   pos(inum.inum, sector + PAGE_SECTORS))? {
                    return Ok(false);
                }

                mapping.lock_folio_create(index, c::GFP_KERNEL).map_err(|_| BchError::from(c::ENOMEM))?
            }
        };

        assert!(start < folio.end_pos() && end > folio.pos(),
                "truncate_folio: {start}..{end} outside the folio at {}..{}", folio.pos(), folio.end_pos());

        let start_offset = (start.max(folio.pos()) - folio.pos()) as usize;
        let end_offset = (end.min(folio.end_pos()) - folio.pos()) as usize;

        /* Folio boundary? Nothing to do */
        if start_offset == 0 && end_offset == folio.size() {
            return Ok(false);
        }

        if !folio.create_state() {
            return Err(c::ENOMEM.into());
        }

        if !folio.uptodate() {
            folio.read_single(mapping, false)?;
        }

        folio.update_state(self)?;

        let block_bytes = fs.block_bytes() as usize;
        let blocks = start_offset.next_multiple_of(block_bytes) >> 9..(end_offset / block_bytes * block_bytes) >> 9;
        let mut i_sectors_delta = 0;
        for s in folio.lock_state().sectors().get_mut(blocks).into_iter().flatten() {
            s.bits.set_nr_replicas(0);

            i_sectors_delta -= i64::from(s.state() == SectorState::dirty);
            s.set_state(SectorState::unallocated);
        }

        self.i_sectors_acct(None, i_sectors_delta);

        /*
         * Caller needs to know whether this folio will be written out by
         * writeback - doing an i_size update if necessary - or whether it will
         * be responsible for the i_size update.
         *
         * We can see a folio wholly above i_size here even with the inode fully
         * locked: marking a folio dirty doesn't go through i_rwsem (e.g.
         * bio_set_pages_dirty() from a DIO read whose destination buffer is an
         * mmap of this file, completing after the file was truncated), so "no
         * dirty folios above i_size" isn't an invariant we maintain.
         * __bch2_writepage cleans up the per-sector dirty bits when it sees
         * above-i_size data; here we just need to not walk off the end of the
         * folio when computing end_pos.
         */
        let i_size = self.vfs().size();
        let mut end_pos = folio.end_pos();
        if i_size > folio.pos() {
            end_pos = end_pos.min(i_size);
        }
        let ret = folio.lock_state()
            .sectors()[((end_pos - 1 - folio.pos()) >> 9) as usize].has_data();

        folio.zero_segment(start_offset, end_offset);

        /*
         * Bit of a hack - we don't want truncate to fail due to -ENOSPC.
         *
         * XXX: because we aren't currently tracking whether the folio has actual
         * data in it (vs. just 0s, or only partially written) this wrong. ick.
         */
        folio.reserve_writeback(self)
            .expect("truncate_folio: a disk reservation that doesn't check for ENOSPC");

        /*
         * This removes any writeable userspace mappings; we need to force
         * .page_mkwrite to be called again before any mmapped writes, to
         * redirty the full page:
         */
        folio.mkclean();
        mapping.dirty_folio(*folio);
        Ok(ret)
    }

    fn truncate_folio_at(&self, from: u64) -> Result<bool, BchError> {
        self.truncate_folio(from >> PAGE_SHIFT, from, u64::MAX)
    }

    fn truncate_folios(&self, start: u64, end: u64) -> Result<bool, BchError> {
        let mut ret = self.truncate_folio(start >> PAGE_SHIFT, start, end)?;

        if start >> PAGE_SHIFT != end >> PAGE_SHIFT {
            ret = self.truncate_folio((end - 1) >> PAGE_SHIFT, start, end)?;
        }
        Ok(ret)
    }

    fn extend(&self, idmap: MntIdmap<'_>, inode_u: &c::bch_inode_unpacked, attr: &c::iattr)
        -> Result<(), BchError>
    {
        self.zero_pagecache_posteof();

        /*
         * sync appends:
         *
         * this has to be done _before_ extending i_size:
         */
        self.vfs().mapping().write_and_wait_range(inode_u.bi_size, u64::MAX)?;

        self.vfs().truncate_setsize(attr.ia_size as u64);

        self.setattr_nonsize(idmap, Iattr::of(attr))
    }

    pub(crate) fn truncate(&self, idmap: MntIdmap<'_>, attr: &mut c::iattr) -> Result<(), BchError> {
        use c::InodeInfoFlag::{EI_INODE_ERROR, EI_INODE_FLUSH_ON_CLOSE};

        let fs = self.fs();
        let v = self.vfs();
        let mapping = v.mapping();
        let new_size = attr.ia_size as u64;
        let mut i_sectors_delta = 0;

        /*
         * If the truncate call with change the size of the file, the
         * cmtimes should be updated. If the size will not change, we
         * do not need to update the cmtimes.
         */
        if new_size != v.size() {
            if attr.ia_valid & ATTR_MTIME == 0 {
                attr.ia_mtime = ktime_get_coarse_real_ts64();
            }
            if attr.ia_valid & ATTR_CTIME == 0 {
                attr.ia_ctime = ktime_get_coarse_real_ts64();
            }
            attr.ia_valid |= ATTR_MTIME | ATTR_CTIME;
        }

        v.dio_wait();
        let _block = self.ei_pagecache_lock.lock::<c::PagecacheBlock>();

        let inode_u = inode::find_by_inum(fs, self.inum())?;

        /*
         * check this before next assertion; on filesystem error our normal
         * invariants are a bit broken (truncate has to truncate the page cache
         * before the inode).
         */
        fs.journal.error()?;

        crate::warn_once!(!self.ei_flags.get(EI_INODE_ERROR) && v.size() < inode_u.bi_size, fs,
                          "truncate spotted in mem i_size < btree i_size: {} < {}",
                          v.size(), inode_u.bi_size);

        if new_size > v.size() {
            return self.extend(idmap, &inode_u, attr);
        }

        attr.ia_valid &= !ATTR_SIZE;

        self.truncate_folio_at(new_size)?;

        if new_size == 0 && v.size() != 0 {
            self.ei_flags.set(EI_INODE_FLUSH_ON_CLOSE, true);
        }

        v.truncate_setsize(new_size);

        /*
         * When extending, we're going to write the new i_size to disk
         * immediately so we need to flush anything above the current on disk
         * i_size first:
         *
         * Also, when extending we need to flush the page that i_size currently
         * straddles - if it's mapped to userspace, we need to ensure that
         * userspace has to redirty it and call .mkwrite -> set_page_dirty
         * again to allocate the part of the page that was extended.
         */
        if new_size > inode_u.bi_size {
            mapping.write_and_wait_range(inode_u.bi_size, new_size - 1)?;
        } else if new_size % PAGE_BYTES != 0 {
            mapping.write_and_wait_range(new_size - new_size % PAGE_BYTES, new_size - 1)?;
        }

        let ret = io_misc::truncate(fs, self.inum(), new_size, &mut i_sectors_delta);
        self.i_sectors_acct(None, i_sectors_delta);

        self.clear_reserved();

        /*
         * If we error here, VFS caches are now inconsistent with btree
         */
        ret.inspect_err(|_| self.ei_flags.set(EI_INODE_ERROR, true))?;

        if v.size() == 0 && v.blocks() != 0 && fs.journal.error().is_ok() {
            self.i_blocks_not_zero_at_truncate();
        }

        self.setattr_nonsize(idmap, Iattr::of(attr))
    }

    #[cold]
    #[inline(never)]
    fn i_blocks_not_zero_at_truncate(&self) {
        let fs = self.fs();
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "inode {} truncated to 0 but i_blocks {} (ondisk {})",
             self.inum().inum, self.vfs().blocks(), self.inode().bi_sectors as i64);

        let log = count_fsck_err(fs, id::vfs_inode_i_blocks_not_zero_at_truncate, &mut msg);
        msg.set_suppressed(!log);
    }
}

/* fallocate: */

impl InodeInfo {
    fn fpunch(&self, offset: u64, len: u64) -> Result<(), BchError> {
        let fs = self.fs();
        let end         = offset + len;
        let block_bytes = fs.block_bytes();
        let block_start = offset.next_multiple_of(block_bytes);
        let block_end   = end / block_bytes * block_bytes;

        let truncated_last_page = self.truncate_folios(offset, end)?;

        self.vfs().truncate_pagecache_range(offset, end - 1);

        if block_start < block_end {
            let mut i_sectors_delta = 0;

            let ret = io_misc::fpunch(fs, self.inum(), block_start >> 9, block_end >> 9, &mut i_sectors_delta);
            self.i_sectors_acct(None, i_sectors_delta);
            ret?;
        }

        let lock = self.update_lock();
        if end >= self.vfs().size() && !truncated_last_page {
            lock.write_size(self.vfs().size(), ATTR_MTIME | ATTR_CTIME)
        } else {
            lock.write_inode(ATTR_MTIME | ATTR_CTIME, |t, bi| {
                bi.bi_mtime = t.fs().current_time();
                bi.bi_ctime = bi.bi_mtime;
                Ok(())
            })
        }
    }

    fn fcollapse_finsert(&self, offset: u64, len: u64, insert: bool) -> Result<(), BchError> {
        let fs = self.fs();
        let v = self.vfs();

        if (offset | len) & (fs.block_bytes() - 1) != 0 {
            return fs.throw(Errcode::BCH_ERR_EINVAL_fcollapse_finsert_unaligned);
        }

        if insert {
            if MAX_LFS_FILESIZE - v.size() < len {
                return Err(c::EFBIG.into());
            }

            if offset >= v.size() {
                return fs.throw(Errcode::BCH_ERR_EINVAL_finsert_past_eof);
            }
        } else if offset + len >= v.size() {
            return fs.throw(Errcode::BCH_ERR_EINVAL_fcollapse_past_eof);
        }

        v.mapping().write_invalidate_range(offset, u64::MAX)?;

        if insert {
            v.set_size(v.size() + len);
        }

        let mut i_sectors_delta = 0;
        let ret = io_misc::fcollapse_finsert(fs, self.inum(), offset >> 9, len >> 9,
                                             insert, &mut i_sectors_delta);
        if ret.is_ok() && !insert {
            v.set_size(v.size() - len);
        }
        self.i_sectors_acct(None, i_sectors_delta);

        ret
    }

    fn fallocate_sectors(&self, mode: u32, start_sector: u64, end_sector: u64) -> Result<(), BchError> {
        let fs = self.fs();
        let inum = self.inum();
        let end_pos = pos(inum.inum, end_sector);
        let opts = inode_opts::get_inode(fs, &self.inode());
        let zero_range = mode & c::FALLOC_FL_ZERO_RANGE != 0;
        let mapping = self.vfs().mapping();

        let trans = crate::btree_trans!(fs);
        let mut iter = BtreeIter::new(&trans, Btree::extents, pos(inum.inum, start_sector),
                                      BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT | BtreeIterFlags::TRIM);
        let mut ret = Ok(());

        while ret.is_ok() {
            let step = lockrestart_do(&trans, |t| -> Result<bool, BchError> {
                // each try, not each step: a step that restarts may have moved
                // @iter to the end
                if bkey_ge(iter.pos(), end_pos) {
                    return Ok(true);
                }

                let mut i_sectors_delta = 0;
                let mut quota_res = self.quota_reservation();

                let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;
                iter.set_snapshot(snapshot);

                let Some(k) = iter.peek_slot_max(t, end_pos)? else { return Ok(true) };

                let mut hole_start  = k.k.start_offset();
                let mut hole_end    = k.k.p.offset;
                let is_allocation   = k.k.extent_is_allocation();
                /* already reserved */
                let reserved        = k.extent_is_reservation(fs) &&
                    u32::from(k.durability_safe(fs).nr_overwritable) >= u32::from(opts.data_replicas);
                let is_data         = k.k.extent_is_data();

                if reserved || (is_data && !zero_range) {
                    iter.advance();
                    return Ok(false);
                }

                if !zero_range {
                    /*
                     * Lock ordering - can't be holding btree locks while
                     * blocking on a folio lock:
                     */
                    let min_replicas = u32::from(opts.data_replicas);
                    if mapping.clamp_data_hole(&mut hole_start, &mut hole_end, min_replicas, true).is_err() {
                        t.drop_locks_do(|| {
                            let _ = mapping.clamp_data_hole(&mut hole_start, &mut hole_end, min_replicas, false);
                            Ok(())
                        })?;
                    }
                    iter.set_pos(pos(iter.pos().inode, hole_start));

                    if hole_start == hole_end {
                        return Ok(false);
                    }
                }

                let sectors = hole_end - hole_start;

                if !is_allocation {
                    quota_res.add(sectors, true)?;
                }

                io_misc::extent_fallocate(t, inum, &mut iter, sectors, &opts, &mut i_sectors_delta,
                                          c::write_point_specifier::hashed(current_id()))?;

                self.i_sectors_acct(Some(&mut *quota_res), i_sectors_delta);

                let reserved_end = iter.pos().offset;
                if mapping.mark_reserved(&mut hole_start, reserved_end, true).is_err() {
                    t.drop_locks_do(|| mapping.mark_reserved(&mut hole_start, reserved_end, false))?;
                }
                Ok(false)
            });

            ret = match step {
                Ok(true) => break,
                r        => r.map(|_| ()),
            };
        }

        if zero_range && ret.is_err_and(|e| e.matches(c::ENOSPC)) {
            let mut i_sectors_delta = 0;

            let _ = io_misc::fpunch_at(&trans, &mut iter, inum, end_sector, Some(&mut i_sectors_delta));
            self.i_sectors_acct(None, i_sectors_delta);
        }

        ret
    }

    fn fallocate(&self, mode: u32, offset: u64, len: u64) -> Result<(), BchError> {
        let fs = self.fs();
        let v = self.vfs();
        let zero_range = mode & c::FALLOC_FL_ZERO_RANGE != 0;
        let keep_size  = mode & c::FALLOC_FL_KEEP_SIZE != 0;
        let block_bytes = fs.block_bytes();
        let mut end         = offset + len;
        let mut block_start = offset / block_bytes * block_bytes;
        let mut block_end   = end.next_multiple_of(block_bytes);
        let mut truncated_last_page = false;

        if !keep_size && end > v.size() {
            v.newsize_ok(end)?;
            self.zero_pagecache_posteof();
        }

        if zero_range {
            truncated_last_page = self.truncate_folios(offset, end)?;

            v.truncate_pagecache_range(offset, end - 1);

            block_start = offset.next_multiple_of(block_bytes);
            block_end   = end / block_bytes * block_bytes;
        }

        let ret = self.fallocate_sectors(mode, block_start >> 9, block_end >> 9);

        /*
         * On -ENOSPC in ZERO_RANGE mode, we still want to do the inode update,
         * so that the VFS cache i_size is consistent with the btree i_size:
         */
        if ret.is_err_and(|e| !(zero_range && e.matches(c::ENOSPC))) {
            return ret;
        }

        if keep_size && end > v.size() {
            end = v.size();
        }

        let mut ret2 = Ok(());
        if end >= v.size() && ((zero_range && !truncated_last_page) || !keep_size) {
            v.size_extend(end);

            ret2 = self.update_lock().write_size(end, 0);
        }

        ret.and(ret2)
    }
}

pub(super) fn fallocate(inode: &InodeInfo, file: &File, mode: c_int, offset: i64, len: i64)
    -> Result<(), BchError>
{
    let fs = inode.fs();
    let (mode, offset, len) = (mode as u32, offset as u64, len as u64);

    let Some(_write) = fs.writes.tryget_live() else {
        return Err(c::EROFS.into());
    };

    let _i_rwsem = inode.vfs().lock();
    inode.vfs().dio_wait();
    let _block = inode.ei_pagecache_lock.lock::<c::PagecacheBlock>();

    file.modified()?;

    let ret = if mode & !(c::FALLOC_FL_KEEP_SIZE | c::FALLOC_FL_ZERO_RANGE) == 0 {
        inode.fallocate(mode, offset, len)
    } else if mode == c::FALLOC_FL_PUNCH_HOLE | c::FALLOC_FL_KEEP_SIZE {
        inode.fpunch(offset, len)
    } else if mode == c::FALLOC_FL_INSERT_RANGE {
        inode.fcollapse_finsert(offset, len, true)
    } else if mode == c::FALLOC_FL_COLLAPSE_RANGE {
        inode.fcollapse_finsert(offset, len, false)
    } else {
        fs.throw(Errcode::BCH_ERR_unsupported_fallocate_mode)
    };

    inode.clear_reserved();
    ret
}

/* fseek: */

fn seek_data(inode: &InodeInfo, file: &File, offset: u64) -> Result<u64, BchError> {
    let inum = inode.inum();
    let mut next_data = MAX_LFS_FILESIZE;

    let isize = inode.vfs().size();
    if offset >= isize {
        return Err(c::ENXIO.into());
    }

    let trans = crate::btree_trans!(inode.fs());
    let mut iter = BtreeIter::new(&trans, Btree::extents, pos(inum.inum, offset >> 9), BtreeIterFlags::TRIM);
    subvolume::for_each_in_subvolume_max(&trans, &mut iter, pos(inum.inum, u64::MAX),
                                                  inum.subvol as u32, BtreeIterFlags::TRIM, |_, _, k| {
        if k.k.extent_is_data() {
            next_data = offset.max(k.k.start_offset() << 9);
            Ok(ControlFlow::Break(()))
        } else if k.k.p.offset << 9 > isize {
            Ok(ControlFlow::Break(()))
        } else {
            Ok(ControlFlow::Continue(()))
        }
    })?;

    if next_data > offset {
        next_data = inode.vfs().mapping().seek_data(offset, next_data, 0, false)?;
    }

    if next_data >= isize {
        return Err(c::ENXIO.into());
    }

    file.setpos(next_data, MAX_LFS_FILESIZE)
}

fn seek_hole(inode: &InodeInfo, file: &File, mut offset: u64) -> Result<u64, BchError> {
    let inum = inode.inum();
    let mut next_hole = MAX_LFS_FILESIZE;

    let isize = inode.vfs().size();
    if offset >= isize {
        return Err(c::ENXIO.into());
    }

    let flags = BtreeIterFlags::SLOTS | BtreeIterFlags::TRIM;
    let trans = crate::btree_trans!(inode.fs());
    let mut iter = BtreeIter::new(&trans, Btree::extents, pos(inum.inum, offset >> 9), flags);
    subvolume::for_each_in_subvolume_max(&trans, &mut iter, pos(inum.inum, u64::MAX),
                                                  inum.subvol as u32, flags, |t, _, k| {
        if !k.k.extent_is_data() {
            let start_offset = offset.max(k.k.start_offset() << 9);
            let end_offset = MAX_LFS_FILESIZE;

            /*
             * Found a hole in the btree, now make sure it's
             * a hole in the pagecache. We might have to
             * keep searching if this hole is entirely dirty
             * in the page cache:
             */
            t.unlock();
            let pagecache_hole = inode.vfs().mapping().seek_hole(start_offset, end_offset, 0, false)?;
            if pagecache_hole < end_offset {
                next_hole = pagecache_hole;
                return Ok(ControlFlow::Break(()));
            }
        } else {
            offset = offset.max(k.k.start_offset() << 9);
        }
        Ok(ControlFlow::Continue(()))
    })?;

    file.setpos(next_hole.min(isize), MAX_LFS_FILESIZE)
}

pub(super) fn llseek(inode: &InodeInfo, file: &File, offset: i64, whence: c_int) -> Result<u64, BchError> {
    match whence as u32 {
        c::SEEK_SET | c::SEEK_CUR | c::SEEK_END => file.generic_llseek(offset, whence),
        c::SEEK_DATA => seek_data(inode, file, offset as u64),
        c::SEEK_HOLE => seek_hole(inode, file, offset as u64),
        _            => Err(c::EINVAL.into()),
    }
}

/* remap: */

/*
 * Take a quota reservation for unallocated blocks in a given file range
 * Does not check pagecache
 */
fn quota_reserve_range(inode: &InodeInfo, res: &mut QuotaReservation<'_>, start: u64, end: u64)
    -> Result<(), BchError>
{
    let inum = inode.inum();
    let mut sectors = end - start;

    let trans = crate::btree_trans!(inode.fs());
    let mut iter = BtreeIter::new(&trans, Btree::extents, pos(inum.inum, start), BtreeIterFlags::TRIM);
    subvolume::for_each_in_subvolume_max(&trans, &mut iter, pos(inum.inum, end),
                                                  inum.subvol as u32, BtreeIterFlags::TRIM, |_, _, k| {
        if k.k.extent_is_allocation() {
            assert!(u64::from(k.k.size) <= sectors,
                    "quota_reserve_range: {} sectors left of {start}..{end}, at {}", sectors, k.k);
            sectors -= u64::from(k.k.size);
        }
        Ok(())
    })?;

    res.add(sectors, true)
}

pub(super) fn remap_file_range(src: &InodeInfo, file_src: &File, pos_src: i64, file_dst: &File, pos_dst: i64,
                               len: i64, remap_flags: u32) -> Result<u64, BchError> {
    let dst = unsafe { InodeInfo::of_file(file_dst) };
    let fs = src.fs();
    let block_bytes = fs.block_bytes();
    let (pos_src, pos_dst, mut len) = (pos_src as u64, pos_dst as u64, len as u64);

    if remap_flags & !(c::REMAP_FILE_DEDUP | c::REMAP_FILE_ADVISORY) != 0 {
        return fs.throw(Errcode::BCH_ERR_EINVAL_remap_bad_flags);
    }

    if pos_src & (block_bytes - 1) != 0 || pos_dst & (block_bytes - 1) != 0 {
        return fs.throw(Errcode::BCH_ERR_EINVAL_remap_unaligned);
    }

    if core::ptr::eq(src, dst) && pos_src.abs_diff(pos_dst) < len {
        return fs.throw(Errcode::BCH_ERR_EINVAL_remap_overlapping);
    }

    /*
     * bch2_open() refuses to open a file in a read-only subvolume for
     * writing, but that doesn't cover dedupe: may_dedupe_file() admits a
     * destination opened read-only, for its owner or CAP_SYS_ADMIN.
     */
    subvolume::is_ro(fs, dst.inum().subvol as u32)?;

    let _i_rwsems = VfsInode::lock_two_nondirectories(src.vfs(), dst.vfs());
    let _pagecache_block = InodeInfo::pagecache_block_all([src, dst]);

    src.vfs().dio_wait();
    dst.vfs().dio_wait();

    let mut quota_res = dst.quota_reservation();
    let mut i_sectors_delta = 0;

    file_src.remap_range_prep(pos_src, file_dst, pos_dst, &mut len, remap_flags)?;
    if len == 0 {
        return Ok(0);
    }

    let aligned_len = len.next_multiple_of(block_bytes);

    dst.zero_pagecache_posteof();

    dst.vfs().mapping().write_invalidate_range(pos_dst, pos_dst + len - 1)?;

    quota_reserve_range(dst, &mut quota_res, pos_dst >> 9, (pos_dst + aligned_len) >> 9)?;

    if remap_flags & c::REMAP_FILE_DEDUP == 0 {
        let _ = file_dst.update_time();
    }

    src.vfs().mapping().mark_unallocated(pos_src >> 9, (pos_src + aligned_len) >> 9);

    let remapped = reflink::remap_range(fs,
                                        dst.inum(), pos_dst >> 9,
                                        src.inum(), pos_src >> 9,
                                        aligned_len >> 9,
                                        pos_dst + len, &mut i_sectors_delta,
                                        MntIdmap::of(file_src).owner_or_capable(src.vfs()))?;

    /*
     * due to alignment, we might have remapped slightly more than requsted
     */
    let ret = (remapped << 9).min(len);

    dst.i_sectors_acct(Some(&mut *quota_res), i_sectors_delta);

    dst.clear_reserved();
    src.clear_reserved();

    dst.vfs().size_extend(pos_dst + ret);

    if file_dst.is_dsync() {
        dst.flush(&mut 0)?;
    }
    Ok(ret)
}
