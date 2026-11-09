// SPDX-License-Identifier: GPL-2.0

//! Buffered writes: page cache folios reserved, copied into and dirtied, for
//! writeback to write out.

use super::direct_write::direct_write;
use super::folio::{LockedFolio, Mapping};
use super::folio_reservation::FolioReservation;
use super::inode::InodeInfo;
use super::kiocb::Kiocb;
use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::{flags, KBox};
use crate::util::iov_iter::IovIter;
use crate::util::kernel::{cond_resched, current_id, fatal_signal_pending, unlikely, PAGE_SIZE};
use crate::util::ops::IntoRaw;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::c_export;
use kernel::page::PAGE_SHIFT;

fn fgf_set_order(size: usize) -> c::fgf_t {
    unsafe { c::fgf_set_order(size) }
}

/// What write_begin() leaves write_end(): the inode's page cache held for
/// adding to, and the folio's reservation.
struct WriteBegun {
    _add: c::TwoStateGuard<'static, c::PagecacheAdd>,
    res:  FolioReservation<'static>,
}

/* buffered writes: */

/// write_begin(), past the file or kiocb buffered.c's stub takes: the folio
/// @len bytes at @pos are written into, locked, in *@foliop, and reserved for -
/// its reservation in *@fsdata, for write_end().
#[c_export]
pub unsafe fn bch2_buffered_write_begin(mapping: *mut c::address_space, pos: c::loff_t, len: c_uint,
                                        foliop: *mut *mut c::folio, fsdata: *mut *mut c_void) -> c_int {
    let mapping = unsafe { Mapping::from_raw(mapping) };

    unsafe { *fsdata = core::ptr::null_mut() };
    match write_begin(mapping.inode(), mapping, pos as u64, len as usize) {
        Ok((folio, begun)) => {
            unsafe {
                *foliop = folio.into_raw();
                *fsdata = KBox::into_raw(begun).cast();
            }
            0
        }
        Err(e) => e.class(),
    }
}

fn write_begin(inode: &'static InodeInfo, mapping: Mapping, pos: u64, len: usize)
    -> Result<(LockedFolio, KBox<WriteBegun>), BchError>
{
    let begun = KBox::<WriteBegun>::new_uninit(flags::GFP_KERNEL)?;
    let mut begun = KBox::write(begun, WriteBegun {
        res:  FolioReservation::new(inode),
        _add: inode.ei_pagecache_lock.lock::<c::PagecacheAdd>(),
    });

    if pos > inode.v.size() {
        inode.zero_pagecache_posteof();
    }

    let folio = mapping.get_folio(pos >> PAGE_SHIFT, c::FGP_WRITEBEGIN | fgf_set_order(len), mapping.gfp_mask())?;

    let offset = (pos - folio.pos()) as usize;
    let len = len.min((folio.end_pos() - pos) as usize);

    /* If we're writing entire folio, don't need to read it in first: */
    if !folio.uptodate() && !(offset == 0 && len == folio.size()) {
        let isize = inode.v.size();

        if offset == 0 && pos + len as u64 >= isize {
            folio.zero_segment(len, folio.size());
            folio.flush_dcache();
        } else if folio.pos() >= isize {
            folio.zero_segments(0, offset, offset + len, folio.size());
            folio.flush_dcache();
        } else {
            folio.read_single(mapping, true)?;
        }
    }

    loop {
        folio.update_state(inode)?;

        match begun.res.add(*folio, offset, len) {
            Ok(()) => break,
            /*
             * If the folio hasn't been read in, we won't know if we
             * actually need a reservation - we don't actually need
             * to read here, we just need to check if the folio is
             * fully backed by uncompressed data:
             */
            Err(_) if !folio.uptodate() => folio.read_single(mapping, true)?,
            Err(e) => return Err(e),
        }
    }

    Ok((folio, begun))
}

/// write_end(), past the file or kiocb buffered.c's stub takes: @copied of the
/// @len bytes at @pos written into @folio, write_begin()'s - dirtied, and the
/// file grown to them. How many were.
#[c_export]
pub unsafe fn bch2_buffered_write_end(mapping: *mut c::address_space, pos: c::loff_t, len: c_uint,
                                      copied: c_uint, folio: *mut c::folio, fsdata: *mut c_void) -> c_int {
    let inode = unsafe { Mapping::from_raw(mapping) }.inode();
    let folio = unsafe { LockedFolio::from_raw(folio) };
    let mut begun = unsafe { KBox::from_raw(fsdata.cast::<WriteBegun>()) };
    let (pos, len, mut copied) = (pos as u64, len as usize, copied as usize);
    let offset = (pos - folio.pos()) as usize;

    assert!(offset + copied <= folio.size(), "write_end() of {copied} bytes at {offset} of a {} byte folio",
            folio.size());

    if unlikely(copied < len && !folio.uptodate()) {
        /*
         * The folio needs to be read in, but that would destroy
         * our partial write - simplest thing is to just force
         * userspace to redo the write:
         */
        folio.zero_range(0, folio.size());
        folio.flush_dcache();
        copied = 0;
    }

    inode.v.size_extend(pos + copied as u64);

    if copied != 0 {
        if !folio.uptodate() {
            folio.mark_uptodate();
        }

        begun.res.dirty(*folio, offset, copied);

        inode.ei_last_dirtied.set(current_id() as _);
    }

    drop(folio);
    drop(begun);

    copied as c_int
}

/// @len bytes of @iter copied in at @pos, as far as the folios there could be
/// reserved and copied into: how many were.
fn buffered_write_folios(inode: &InodeInfo, mapping: Mapping, iter: &mut IovIter<'_>, pos: u64, len: usize)
    -> Result<usize, BchError>
{
    assert!(len != 0);

    let mut res = FolioReservation::new(inode);
    let mut last_folio_pos = inode.v.size();

    let ret = copy_into_folios(inode, mapping, iter, pos, len, &mut res, &mut last_folio_pos);

    /*
     * If the last folio added to the mapping starts beyond current EOF, we
     * performed a short write but left around at least one post-EOF folio.
     * Clean up the mapping before we return.
     */
    if last_folio_pos >= inode.v.size() {
        inode.v.truncate_pagecache(inode.v.size());
    }

    ret
}

fn copy_into_folios(inode: &InodeInfo, mapping: Mapping, iter: &mut IovIter<'_>, pos: u64, len: usize,
                    res: &mut FolioReservation<'_>, last_folio_pos: &mut u64) -> Result<usize, BchError> {
    let mut end = pos + len as u64;

    let mut fs = mapping.get_contig_folios(pos, end, c::FGP_WRITEBEGIN | fgf_set_order(len), mapping.gfp_mask())?;
    assert!(!fs.is_empty());

    let f = *fs[0];
    if pos != f.pos() && !f.uptodate() {
        f.read_single(mapping, true)?;
    }

    let f = *fs[fs.len() - 1];
    end = end.min(f.end_pos());
    *last_folio_pos = f.pos();
    if end != f.end_pos() && !f.uptodate() {
        if end >= inode.v.size() {
            f.zero_range(0, f.size());
        } else {
            f.read_single(mapping, true)?;
        }
    }

    inode.read_folio_state(&fs)?;

    let mut f_pos = pos;
    let mut f_offset = (pos - fs[0].pos()) as usize;
    for i in 0..fs.len() {
        let f = *fs[i];
        let f_len = (end.min(f.end_pos()) - f_pos) as usize;

        match res.add_partial(f, f_offset, f_len) {
            Ok(reserved) if reserved == f_len => {}
            Err(e) if i == 0 => return Err(e),
            Err(_) => {
                fs.truncate(i);
                end = end.min(fs[fs.len() - 1].end_pos());
                break;
            }
            Ok(reserved) => {
                if !f.uptodate() {
                    f.read_single(mapping, true)?;
                }

                fs.truncate(i + 1);
                end = f_pos + reserved as u64;
                break;
            }
        }

        f_pos = f.end_pos();
        f_offset = 0;
    }

    if mapping.writably_mapped() {
        for f in fs.iter() {
            f.flush_dcache();
        }
    }

    let mut copied = 0;
    f_pos = pos;
    f_offset = (pos - fs[0].pos()) as usize;
    for i in 0..fs.len() {
        let f = *fs[i];
        let f_len = (end.min(f.end_pos()) - f_pos) as usize;
        let f_copied = f.copy_from_iter_atomic(f_offset, f_len, iter);
        if f_copied == 0 {
            fs.truncate(i);
            break;
        }

        if !f.uptodate() && f_copied != f.size() && pos + ((copied + f_copied) as u64) < inode.v.size() {
            iter.revert(f_copied);
            f.zero_range(0, f.size());
            fs.truncate(i);
            break;
        }

        f.flush_dcache();
        copied += f_copied;

        if f_copied != f_len {
            fs.truncate(i + 1);
            break;
        }

        f_pos = f.end_pos();
        f_offset = 0;
    }

    if copied == 0 {
        return Ok(0);
    }

    end = pos + copied as u64;
    inode.v.size_extend(end);

    f_pos = pos;
    f_offset = (pos - fs[0].pos()) as usize;
    for f in fs.iter() {
        let f_len = (end.min(f.end_pos()) - f_pos) as usize;

        if !f.uptodate() {
            f.mark_uptodate();
        }

        res.dirty(**f, f_offset, f_len);

        f_pos = f.end_pos();
        f_offset = 0;
    }

    inode.ei_last_dirtied.set(current_id() as _);

    Ok(copied)
}

fn buffered_write(k: &Kiocb<'_>, iter: &mut IovIter<'_>) -> Result<usize, BchError> {
    let mapping = k.file().mapping();
    let inode = k.inode();
    let mut pos = k.pos();
    let mut written = 0;
    let mut ret = Ok(());

    let _add = inode.ei_pagecache_lock.lock::<c::PagecacheAdd>();

    if pos > inode.v.size() {
        inode.zero_pagecache_posteof();
    }

    while iter.count() != 0 {
        let offset = (pos & (PAGE_SIZE as u64 - 1)) as usize;
        let mut bytes = iter.count();

        let copied = loop {
            /*
             * Bring in the user page that we will copy from _first_.
             * Otherwise there's a nasty deadlock on copying from the
             * same page as we're writing to, without it being marked
             * up-to-date.
             *
             * Not only is this an optimisation, but it is also required
             * to check that the address is actually valid, when atomic
             * usercopies are used, below.
             */
            if unlikely(iter.fault_in_readable(bytes) != 0) {
                bytes = iter.count().min(PAGE_SIZE - offset);

                if unlikely(iter.fault_in_readable(bytes) != 0) {
                    break Err(BchError::from_raw(c::EFAULT as i32));
                }
            }

            if unlikely(fatal_signal_pending()) {
                break Err(BchError::from_raw(c::EINTR as i32));
            }

            let copied = match buffered_write_folios(inode, mapping, iter, pos, bytes) {
                Ok(copied) => copied,
                Err(e) => break Err(e),
            };

            cond_resched();

            if unlikely(copied == 0) {
                /*
                 * If we were unable to copy any data at all, we must
                 * fall back to a single segment length write.
                 *
                 * If we didn't fallback here, we could livelock
                 * because not all segments in the iov can be copied at
                 * once without a pagefault.
                 */
                bytes = (PAGE_SIZE - offset).min(iter.single_seg_count());
                continue;
            }
            break Ok(copied);
        };

        match copied {
            Ok(copied) => {
                pos += copied as u64;
                written += copied;
            }
            Err(e) => {
                ret = Err(e);
                break;
            }
        }

        mapping.balance_dirty_pages_ratelimited();
    }

    if written != 0 {
        inode.queue_writeback();
        return Ok(written);
    }
    ret.map(|()| 0)
}

pub(crate) unsafe extern "C" fn bch2_write_iter(iocb: *mut c::kiocb, from: *mut c::iov_iter) -> isize {
    let (mut k, mut it) = unsafe { (Kiocb::from_raw(iocb), IovIter::from_raw(from)) };

    if k.direct() {
        return direct_write(&mut k, &mut it);
    }

    let inode = k.inode();
    let fs = inode.fs();

    /*
     * Buffered write: dirties page cache; must be serialized against snapshot
     * creation so the snapshot doesn't capture a half-flushed inconsistent
     * state. O_DIRECT doesn't need this — btree commits are already atomic
     * w.r.t. snapshot — so we take the lock only on this side of the branch.
     *
     * NB: this assumes buffered writes are synchronous — submit and dirty
     * happen in the same task. If async buffered IO is ever added (e.g.
     * io_uring buffered writes via worker threads), the up_read here will
     * happen in a different task than down_read, which percpu_rwsem's lockdep
     * tracking won't tolerate. At that point switch this lock to a plain
     * rwsem + down_read_non_owner / up_read_non_owner under CONFIG_LOCKDEP,
     * or rethink the model.
     */
    let ret = {
        let _locked = inode.v.lock();
        let _create_lock = fs.snapshots.create_lock.read();

        (|| {
            if k.write_checks(&mut it)? == 0 {
                return Ok(0);
            }

            k.file().remove_privs()?;
            k.file().update_time()?;

            let written = buffered_write(&k, &mut it)?;
            k.advance(written as u64);
            Ok::<_, BchError>(written)
        })()
    };

    match ret {
        Ok(0)       => 0,
        Ok(written) => k.write_sync(written).into_raw(),
        Err(e)      => e.class() as isize,
    }
}
