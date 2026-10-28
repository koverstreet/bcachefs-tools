// SPDX-License-Identifier: GPL-2.0

//! FIEMAP: a file's mappings, from @start to @end, in sectors.
//!
//! A mapping is an extent from the btree, or dirty data in the pagecache over
//! a hole in it, reported as delalloc. A reflink_p is reported as the part of
//! the indirect extent it points to, at the file's offsets. Each mapping is
//! emitted once the next is known, so the last can carry FIEMAP_EXTENT_LAST.
//!
//! Where both have something, the btree's extent wins if it starts first. It
//! should be the pagecache's - we're COW, so dirty data is delalloc - but
//! bch_folio_state has no per-sector writeback tracking, and clean cached data
//! would be reported delalloc too. Even then fiemap couldn't report stable
//! mappings: copygc and rebalance move data, and nothing lets userspace lock
//! that out.
//!
//! Folio locks can't be taken holding btree locks. The pagecache is scanned
//! nonblocking first, then with the locks dropped. That races with writeback
//! inserting extents, but so does any fiemap without FIEMAP_FLAG_SYNC: the
//! range is reported delalloc either way. The transaction is unlocked before
//! each mapping is emitted, since the copy to userspace may fault.
//!
//! C faked a delalloc extent key, with one zeroed pointer, so it could emit
//! dirty data the same way as an extent. Here it's a variant of Mapping
//! instead.

use core::ops::Range;

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, spos, BkeySC};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, TransAttempt};
use crate::c;
use crate::data::extents::{BkeyVExtents, Extents};
use crate::data::ptrs::{BkeyVPtrs, PtrsMut};
use crate::data::reflink::lookup_indirect_extent;
use crate::errcode::{bch_errcode, ret_to_result, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::snapshots::subvolume;
use cstruct_macros::c_extern;

use super::inode::InodeInfo;

c_extern! {
    // pagecache.c's, until it's Rust:
    fn bch2_seek_pagecache_data(vinode: *mut c::inode, start: c::loff_t, end: c::loff_t,
                                state: core::ffi::c_uint, nonblock: bool) -> c::loff_t;
    fn bch2_seek_pagecache_hole(vinode: *mut c::inode, start: c::loff_t, end: c::loff_t,
                                state: core::ffi::c_uint, nonblock: bool) -> c::loff_t;
}

enum Mapping {
    Extent(BkeyBuf),
    Delalloc(Range<u64>),
}

impl Mapping {
    fn sectors(&self) -> Range<u64> {
        match self {
            Mapping::Extent(k) => k.as_ref().k.start_offset()..k.as_ref().k.p.offset,
            Mapping::Delalloc(r) => r.clone(),
        }
    }

    /// True if @info is full, which ends the walk.
    fn emit(&self, fs: &Fs, info: &mut c::fiemap_extent_info, flags: u32) -> Result<bool, BchError> {
        use c::bch_bkey_type as t;

        let mut fill = |logical: u64, physical: u64, len: u64, flags: u32| {
            ret_to_result(unsafe { c::fiemap_fill_next_extent(info, logical << 9, physical << 9,
                                                             len << 9, flags) })
                .map(|full| full != 0)
        };
        let block_sectors = fs.block_bytes() >> 9;
        let unaligned = |sectors: u64| sectors & (block_sectors - 1) != 0;

        let k = match self {
            Mapping::Extent(k) => BkeySC::from(k),
            Mapping::Delalloc(r) => {
                let len = r.end - r.start;
                let flags = flags | c::FIEMAP_EXTENT_DELALLOC |
                    if unaligned(len) { c::FIEMAP_EXTENT_NOT_ALIGNED } else { 0 };
                return fill(r.start, 0, len, flags);
            }
        };
        let (start, len) = (k.k.start_offset(), k.k.size as u64);
        assert!(len != 0, "fiemap: an empty mapping: {}", k.k);

        if let Some(p) = k.arm::<PtrsMut>() {
            let flags = flags |
                if k.k.is::<c::ReflinkV>() { c::FIEMAP_EXTENT_SHARED } else { 0 };

            for d in p.ptrs_decoded(fs) {
                let mut flags = flags;
                let mut offset = d.p.ptr.offset();

                if d.p.ptr.unwritten() != 0 {
                    flags |= c::FIEMAP_EXTENT_UNWRITTEN;
                }
                if d.p.crc.compression_type != 0 {
                    flags |= c::FIEMAP_EXTENT_ENCODED;
                } else {
                    offset += u64::from(d.p.crc.offset);
                }
                if unaligned(offset) || unaligned(len) {
                    flags |= c::FIEMAP_EXTENT_NOT_ALIGNED;
                }

                if fill(start, offset, len, flags)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }

        match k.k.key_type() {
            t::KEY_TYPE_inline_data | t::KEY_TYPE_indirect_inline_data =>
                fill(start, 0, len, flags | c::FIEMAP_EXTENT_DATA_INLINE),
            t::KEY_TYPE_reservation =>
                fill(start, 0, len, flags | c::FIEMAP_EXTENT_DELALLOC | c::FIEMAP_EXTENT_UNWRITTEN),
            t::KEY_TYPE_error => Ok(false),
            _ => {
                crate::warn_once!(true, fs, "fiemap: unhandled key type: {}", k.k);
                Ok(false)
            }
        }
    }
}

/// The first run of dirty data in the pagecache in [@start, @end), in
/// sectors, or an empty range at @end if there's none.
fn pagecache_dirty(t: &TransAttempt<'_, '_>, inode: &InodeInfo, start: u64, end: u64)
    -> Result<Range<u64>, BchError>
{
    let vinode = inode.vfs().as_ptr();
    let (start, end) = ((start << 9) as c::loff_t, (end << 9) as c::loff_t);
    let loff = |r: c::loff_t| if r < 0 { Err(BchError::from_raw(-r as i32)) } else { Ok(r) };

    let scan = |nonblock| {
        // SAFETY: an inode the VFS holds
        let data = loff(unsafe { bch2_seek_pagecache_data(vinode, start, end, 0, nonblock) })?;
        if data == end {
            return Ok(end as u64 >> 9..end as u64 >> 9);
        }
        let hole = loff(unsafe { bch2_seek_pagecache_hole(vinode, data, end, 0, nonblock) })?;
        assert!(data != hole, "fiemap: pagecache data at {data} with a hole at the same offset");
        Ok(data as u64 >> 9..hole as u64 >> 9)
    };

    match scan(true) {
        Err(e) if e == BchError::from(c::EAGAIN) => t.drop_locks_do(|| scan(false)),
        r => r,
    }
}

fn next_mapping(t: &TransAttempt<'_, '_>, inode: &InodeInfo, start: u64, end: u64)
    -> Result<Mapping, BchError>
{
    let fs = t.fs();
    let inum = inode.inum();
    let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;

    let mut iter = BtreeIter::new(t, c::btree_id::extents, spos(inum.inum, start, snapshot),
                                  BtreeIterFlags::TRIM);
    let k = iter.peek_max(t, pos(inum.inum, end))?;

    // Before the btree's extent, if there is one: so it's never cut
    let dirty = pagecache_dirty(t, inode, start, k.map_or(end, |k| k.k.start_offset()))?;

    let k = match k {
        Some(k) if k.k.start_offset() <= dirty.start => k,
        _ => return Ok(Mapping::Delalloc(dirty)),
    };
    let Some(p) = k.arm::<c::ReflinkP>() else {
        return Ok(Mapping::Extent(BkeyBuf::from_key(k)));
    };

    let (start, sectors) = (k.k.start_offset(), k.k.size);
    let mut offset_into_extent = 0;
    let mut indirect_iter = BtreeIter::uninit();
    let indirect = lookup_indirect_extent(t, &mut indirect_iter, &mut offset_into_extent, p, true,
                                          BtreeIterFlags::empty())?;
    if indirect.k.is_deleted() {
        return fs.throw(bch_errcode::BCH_ERR_missing_indirect_extent);
    }

    let mut buf = BkeyBuf::from_key(indirect);
    let k = buf.as_mut();
    let offset_into_extent = offset_into_extent as u64;
    let sectors = sectors.min(k.k.size - offset_into_extent as u32);

    let cut = pos(k.k.p.inode, k.k.start_offset() + offset_into_extent);
    k.arm::<Extents>().expect("an indirect extent is an extent").cut_front_in_place(fs, cut);
    k.k.resize(sectors);
    k.k.p = spos(inum.inum, start + sectors as u64, snapshot);
    Ok(Mapping::Extent(buf))
}

pub(crate) fn fiemap(inode: &InodeInfo, info: &mut c::fiemap_extent_info, start: u64, mut len: u64)
    -> Result<(), BchError>
{
    let fs = inode.fs();
    ret_to_result_void(unsafe { c::fiemap_prep(inode.vfs().as_ptr(), info, start, &mut len, 0) })?;

    let Some(end) = start.checked_add(len) else {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_fiemap_overflow);
    };
    let (mut start, end) = (start >> 9, end >> 9);

    let trans = crate::btree_trans!(fs);
    let mut prev: Option<Mapping> = None;

    while start < end {
        let cur = lockrestart_do(&trans, |t| next_mapping(t, inode, start, end))?;
        let s = cur.sectors();
        assert!(start <= s.start && s.end <= end,
                "fiemap: mapping {s:?} outside the window {start}..{end}");

        if s.start == end {
            break;
        }
        start = s.end;

        if let Some(prev) = prev.replace(cur) {
            trans.unlock();
            if prev.emit(fs, info, 0)? {
                return Ok(());
            }
        }
    }

    if let Some(prev) = prev {
        trans.unlock();
        prev.emit(fs, info, c::FIEMAP_EXTENT_LAST)?;
    }
    Ok(())
}
