// SPDX-License-Identifier: GPL-2.0

//! Buffered writes: page cache folios reserved, copied into and dirtied, for
//! writeback to write out.

use super::direct_write::direct_write;
use super::folio::{FolioSpan, LockedFolio, Mapping};
use super::folio_reservation::FolioReservation;
use super::inode::InodeInfo;
use super::kiocb::Kiocb;
use super::pagecache::ContigFolios;
use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::{flags, KBox};
use crate::util::iov_iter::IovIter;
use crate::util::kernel::{cond_resched, current_id, fatal_signal_pending, likely, unlikely, PAGE_SIZE};
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

    let span = FolioSpan::new(*folio, pos, pos + len as u64);
    let FolioSpan { offset, len, .. } = span;

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

    folio.update_state(inode)?;
    begun.res.add(span)?;

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

        begun.res.dirty(FolioSpan { folio: *folio, offset, len: copied });

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
    let folios = mapping.get_contig_folios(pos, pos + len as u64, c::FGP_WRITEBEGIN | fgf_set_order(len),
                                           mapping.gfp_mask())?;
    let last_folio_pos = folios.last().pos();

    // by value: unlocked when it returns, as the truncate below waits on them
    let ret = write_contig_folios(inode, mapping, iter, folios, &mut res);

    /*
     * If the last folio added to the mapping starts beyond current EOF, we
     * performed a short write but left around at least one post-EOF folio.
     * Clean up the mapping before we return.
     */
    let isize = inode.v.size();
    if last_folio_pos >= isize {
        inode.v.truncate_pagecache(isize);
    }
    ret
}

fn write_contig_folios(inode: &InodeInfo, mapping: Mapping, iter: &mut IovIter<'_>, mut w: ContigFolios,
                       res: &mut FolioReservation<'_>) -> Result<usize, BchError> {
    w.read_partial(inode, mapping)?;
    inode.read_folio_state(w.folios())?;
    w.reserve(res, mapping)?;

    if mapping.writably_mapped() {
        w.flush_dcache();
    }

    let copied = w.copy_from_iter(inode, iter);
    if copied == 0 {
        return Ok(0);
    }

    inode.v.size_extend(w.end());
    w.dirty(res);
    inode.ei_last_dirtied.set(current_id() as _);

    Ok(copied)
}

impl ContigFolios {
    /// Its partial folios, not uptodate, read in - or the last zeroed, if it's
    /// past EOF - for a write into part of them.
    fn read_partial(&self, inode: &InodeInfo, mapping: Mapping) -> Result<(), BchError> {
        if let Some(f) = self.head().filter(|f| !f.uptodate()) {
            f.read_single(mapping, true)?;
        }

        if let Some(f) = self.tail().filter(|f| !f.uptodate()) {
            if self.end() >= inode.v.size() {
                f.zero_range(0, f.size());
            } else {
                f.read_single(mapping, true)?;
            }
        }
        Ok(())
    }

    /// Reserved for in @res, and cut to where that ran out - an error only if
    /// none of it could be; a folio reserved for in part is read in.
    fn reserve(&mut self, res: &mut FolioReservation<'_>, mapping: Mapping) -> Result<(), BchError> {
        let end = 'reserve: {
            for s in self.spans() {
                match res.add_partial(s) {
                    Ok(reserved) if reserved == s.len => {}
                    Err(e) if s.folio == self.first() => return Err(e),
                    Err(_) => break 'reserve s.pos(),
                    Ok(reserved) => {
                        if !s.folio.uptodate() {
                            s.folio.read_single(mapping, true)?;
                        }
                        break 'reserve s.pos() + reserved as u64;
                    }
                }
            }
            self.end()
        };
        self.truncate(end);
        Ok(())
    }

    /// @iter copied in, without faulting, and cut to what was: how much.
    fn copy_from_iter(&mut self, inode: &InodeInfo, iter: &mut IovIter<'_>) -> usize {
        let mut copied = 0;
        for s in self.spans() {
            let f = s.folio;
            let f_copied = s.copy_from_iter_atomic(iter);
            if f_copied == 0 {
                break;
            }

            if !f.uptodate() && f_copied != f.size() {
                if self.pos() + ((copied + f_copied) as u64) < inode.v.size() {
                    iter.revert(f_copied);
                    f.zero_range(0, f.size());
                    break;
                }

                /*
                 * Past EOF it's kept, and marked uptodate when it's dirtied:
                 * what wasn't copied over is a new folio's uninitialized
                 * memory, unless it's zeroed
                 */
                f.zero_segments(0, s.offset, s.offset + f_copied, f.size());
            }

            f.flush_dcache();
            copied += f_copied;

            if f_copied != s.len {
                break;
            }
        }

        self.truncate(self.pos() + copied as u64);
        copied
    }

    /// Uptodate, and dirtied with what @res reserved for them.
    fn dirty(&self, res: &mut FolioReservation<'_>) {
        for s in self.spans() {
            if !s.folio.uptodate() {
                s.folio.mark_uptodate();
            }

            res.dirty(s);
        }
    }
}

/// @iter copied in at @pos, as much as one go at it could: how much was.
fn buffered_write_chunk(inode: &InodeInfo, mapping: Mapping, iter: &mut IovIter<'_>, pos: u64)
    -> Result<usize, BchError>
{
    let offset = (pos & (PAGE_SIZE as u64 - 1)) as usize;
    let mut bytes = iter.count();

    loop {
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
                return Err(c::EFAULT.into());
            }
        }

        if unlikely(fatal_signal_pending()) {
            return Err(c::EINTR.into());
        }

        let copied = buffered_write_folios(inode, mapping, iter, pos, bytes)?;

        cond_resched();

        if likely(copied != 0) {
            return Ok(copied);
        }

        /*
         * If we were unable to copy any data at all, we must
         * fall back to a single segment length write.
         *
         * If we didn't fallback here, we could livelock
         * because not all segments in the iov can be copied at
         * once without a pagefault.
         */
        bytes = (PAGE_SIZE - offset).min(iter.single_seg_count());
    }
}

fn buffered_write(k: &Kiocb<'_>, iter: &mut IovIter<'_>) -> Result<usize, BchError> {
    let mapping = k.file().mapping();
    let inode = k.inode();
    let mut written = 0;

    let _add = inode.ei_pagecache_lock.lock::<c::PagecacheAdd>();

    if k.pos() > inode.v.size() {
        inode.zero_pagecache_posteof();
    }

    while iter.count() != 0 {
        match buffered_write_chunk(inode, mapping, iter, k.pos() + written as u64) {
            Ok(copied) => written += copied,
            Err(e) if written == 0 => return Err(e),
            Err(_) => break,
        }

        mapping.balance_dirty_pages_ratelimited();
    }

    if written != 0 {
        inode.queue_writeback();
    }
    Ok(written)
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

    ret.and_then(|written| if written != 0 { k.write_sync(written) } else { Ok(0) }).into_raw()
}
