// SPDX-License-Identifier: GPL-2.0

/*
 * readpage(s):
 *
 * A readahead bio covers the folios the readahead control asked for, plus
 * whatever extend() adds to reach an extent boundary - partial reads of a
 * checksummed or compressed extent are expensive. Only that extension is
 * speculative, and only it may be abandoned.
 *
 * Abandoning it is worth doing because extend() drops btree locks to allocate
 * folios: under write load we lose the relock repeatedly to an extent that
 * keeps moving, and holding the transaction open to chase it costs more than
 * the readahead is worth.
 *
 * The complication is that reads are sector granular and the page cache is
 * folio granular, so where we stop issuing is generally not a folio boundary.
 * Every folio has to be unlocked exactly once, by whoever owns it: us for the
 * folios we abandon, the read completion for the folios we issued. The folio
 * we stop inside belongs to the completion - IO is landing in it - but it
 * isn't uptodate, and folio_end_read() can't express that. bch_folio.
 * partially_uptodate does.
 */

use super::folio::{Folio, Mapping, Readahead};
use super::inode::InodeInfo;
use super::pagecache;
use crate::btree::bkey::{prelude::*, BkeySC};
use crate::btree::iter::TransAttempt;
use crate::c;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::read::{self, ReadHooks, Rbio};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::inode_opts::get_inode;
use crate::util::alloc::{KVVec, Kmalloc};
use crate::util::bio::{bvec_offset, inline_vecs, plugged, Bio, BioBuffersUnmanaged, BioOwner, BioRange, HasBio};
use crate::os::kernel::PAGE_SIZE;
use crate::util::mempool::Mempool;
use crate::util::sleep::Sleep;
use crate::util::task::{exclusive_size, waiter, CompletionReturn, Return, TaskSlot};
use crate::util::workqueue::Queue;
use core::pin::Pin;
use core::ptr::NonNull;
use cstruct_macros::in_place;
use pin_init::{pin_data, pin_init, pinned_drop};

type F = c::ReadFlags;

const PAGE_SECTORS: u64 = (PAGE_SIZE >> 9) as u64;

/// The folios a readahead asked for, each with its bcachefs state: read in
/// order, or given back.
struct Folios {
    folios: KVVec<Folio>,
    idx:    usize,
}

impl Folios {
    fn new() -> Self {
        Folios { folios: KVVec::new(), idx: 0 }
    }

    fn take(ra: &mut Readahead<'_>) -> Result<Self, BchError> {
        let mut f = Folios::new();
        while let Some(folio) = ra.next() {
            if !folio.create_state() || f.folios.push(folio).is_err() {
                ra.unread(folio);
                return if f.folios.is_empty() { Err(BchError::from(c::ENOMEM)) } else { Ok(f) };
            }
        }
        Ok(f)
    }

    fn peek(&self) -> Option<Folio> {
        self.folios.get(self.idx).copied()
    }

    fn advance(&mut self) {
        self.idx += 1;
    }

    fn remaining(&self) -> usize {
        self.folios.len() - self.idx
    }

    fn give_back(&self, ra: &mut Readahead<'_>) {
        for &folio in self.folios.iter().rev() {
            ra.unread(folio);
        }
    }
}

/// What a read into the page cache does besides read::read()'s.
struct PagecacheRead {
    /// The readahead's folios to extend into - none but new ones, for a
    /// single folio's read that's extended - or None, not extended
    folios:    Option<NonNull<Folios>>,
    mapping:   Mapping,
    read_from: u64,
    /// The readahead window: past this is the speculative extension
    orig_end:  u64,
}

// The folios are on the stack of the read's caller: the hooks are called only
// in read()'s walk, before its first await, which runs inline in the caller.
unsafe impl Send for PagecacheRead {}

impl PagecacheRead {
    fn new_folio(&self, t: &TransAttempt<'_, '_>, end_sector: u64, sectors_remaining: u32) -> Option<Folio> {
        let mapping = self.mapping;
        if u64::from(sectors_remaining) < PAGE_SECTORS << mapping.min_folio_order() {
            return None;
        }

        let folio_offset = end_sector / PAGE_SECTORS;
        let order = (u64::from(sectors_remaining) / PAGE_SECTORS).ilog2()
            /* ensure proper alignment */
            .min((folio_offset | 1 << 31).trailing_zeros())
            /*
             * mapping_max_folio_order() is 0 on !THP no matter what
             * we asked for at inode init: without THP the MM has no
             * split_folio(), only a stub that warns and returns
             * -EINVAL, so a large folio we put in the page cache
             * here can never be split again.
             *
             * This can't fight the mapping_min_folio_order() check
             * above: a min order above 0 means bs > ps, and
             * bch2_fs_alloc() already refuses to mount such a
             * filesystem on !THP.
             */
            .min(mapping.max_folio_order());

        if mapping.has_folio(folio_offset) {
            return None;
        }

        let enomem = || BchError::from(c::ENOMEM);
        let gfp = mapping.readahead_gfp();
        let mut folio = t.allocate_dropping_locks_norelock(|| mapping.alloc_folio(gfp, order).ok_or_else(enomem)).ok()?;
        t.allocate_dropping_locks_norelock(|| folio.alloc_state().map(drop).ok_or_else(enomem)).ok()?;
        t.allocate_dropping_locks_norelock(|| mapping.add_folio(&mut folio, folio_offset, c::GFP_KERNEL)).ok()?;
        Some(folio.into_cached())
    }
}

impl ReadHooks for PagecacheRead {
    fn read_from(&self) -> u64 {
        self.read_from
    }

    fn extend(&mut self, t: &TransAttempt<'_, '_>, read: &Rbio, k: BkeySC<'_>, sectors: u32)
        -> Result<(), BchError>
    {
        let Some(folios) = self.folios else { return Ok(()) };
        let folios = unsafe { &mut *folios.as_ptr() };
        let get_more = k.arm::<Ptrs>()
            .is_some_and(|kp| kp.crcs(read.fs()).any(|crc| crc.csum_type != c::CsumType::none ||
                                                         crc.compression_type != c::CompressionType::none));
        let mut bio = read.bio.lend();

        loop {
            let (vcnt, max_vecs, iter) = (bio.raw().bi_vcnt, bio.raw().bi_max_vecs, bio.raw().bi_iter);
            let bio_sectors = iter.bi_size >> 9;
            if bio_sectors >= sectors || vcnt >= max_vecs {
                break;
            }
            let end_sector = iter.bi_sector + u64::from(bio_sectors);

            let folio = match folios.peek() {
                Some(folio) => {
                    folios.advance();
                    folio
                }
                None if get_more => match self.new_folio(t, end_sector, sectors - bio_sectors) {
                    Some(folio) => folio,
                    None => break,
                },
                None => break,
            };

            assert_eq!(folio.sector(), end_sector, "a folio added to a read bio, not at its end");
            unsafe { bio.bio_mut().add_folio(folio) };
        }
        drop(bio);

        t.relock()
    }

    fn extent(&mut self, read: &Rbio, range: BioRange<'_>, k: BkeySC<'_>) {
        pagecache::bio_page_state_set(read.fs(), range, k);
    }

    /* everything asked for is issued; the rest isn't worth a retry */
    fn give_up(&mut self, read: &Rbio) -> bool {
        let mut bio = read.bio.lend();
        let iter = bio.raw().bi_iter;
        if self.folios.is_none() || iter.bi_sector < self.orig_end {
            return false;
        }

        let mut keep = iter.bi_idx;
        let partial = bvec_offset(&iter);
        let mut folios = bio.range().folios();
        if partial != 0 {
            let fv = folios.next().expect("a partial bvec's folio");
            unsafe { Folio::from_raw(fv.folio.as_ptr()) }.set_partially_uptodate((partial >> 9) as u16);
            keep += 1;
        }
        for fv in folios {
            unsafe { Folio::from_raw(fv.folio.as_ptr()) }.end_read(false);
        }

        bio.bio_mut().truncate_vecs(keep as u16);
        true
    }
}

/// The folio a read stopped inside, if it did: read, but not uptodate.
fn partial_folio(b: &impl BioOwner) -> Option<Folio> {
    let fv = b.range_all().folios().last()?;
    let folio = unsafe { Folio::from_raw(fv.folio.as_ptr()) };
    (folio.partially_uptodate() != 0).then_some(folio)
}

/// Each of @b's folios but @except unlocked: uptodate if read.
fn end_reads(b: &impl BioOwner, ret: Result<(), BchError>, except: Option<Folio>) {
    /* a stale partially_uptodate would spin do_read_cache_folio() forever: */
    let partial = partial_folio(b);
    for fv in b.range_all().folios() {
        let folio = unsafe { Folio::from_raw(fv.folio.as_ptr()) };
        if Some(folio) != except {
            folio.end_read(ret.is_ok() && Some(folio) != partial);
        }
    }
}

/// A read into the page cache - readahead's, or a single folio's: the home of
/// the task that reads it, its bvecs inline after the box.
#[pin_data(PinnedDrop)]
#[repr(C)]
struct ReadBio {
    #[pin]
    rbio: Rbio,
}
crate::has_bio!(ReadBio, rbio.bio);

#[pinned_drop]
impl PinnedDrop for ReadBio {
    fn drop(self: Pin<&mut Self>) {
        unsafe { self.bio_mut().uninit() }
    }
}

/// A read's home, as its task has it: the bio's owner.
struct ReadHome<'h>(Pin<&'h mut ReadBio>);

unsafe impl BioOwner for ReadHome<'_> {
    type Buffers = BioBuffersUnmanaged;

    fn bio(&self) -> &Bio {
        self.0.rbio.bio()
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        self.0.as_mut().bio_mut()
    }
}

/// A read's box is freed when its task is done, after the read - when the
/// filesystem may be gone: so from a pool of the module's.
static READS: Mempool<Kmalloc> = ::pin_init::zeroed();

/// At module init: elements for the most bvecs a bio has.
pub fn pools_init() -> Result<(), BchError> {
    let size = exclusive_size::<ReadBio, ReadFut<'static>, &'static Mempool<Kmalloc>>(
        inline_vecs(c::BIO_MAX_VECS as u16));
    unsafe { Mempool::init_static(&READS, Mempool::kmalloc(1, size)) }
}

/// # Safety
/// At module exit, after the last box's grace period.
pub unsafe fn pools_exit() {
    unsafe { READS.exit() }
}

/// What's done with a read's folios once it's read.
enum End {
    /// Readahead's: each ended
    Readahead,
    /// A single folio's: those extend() added ended - @folio is the caller's -
    /// and the caller, waiting for it, given what was read
    Single { folio: Folio, extend: bool, r: Return<Result<(), BchError>, waiter::Sync> },
}

// @folio's the caller's, locked, and the caller's waiting on the task
unsafe impl Send for End {}

#[in_place(ReadFut)]
async fn read_task<'h>(home: Pin<&'h mut ReadBio>, inum: c::subvol_inum, hooks: PagecacheRead, end: End) {
    let ret = read::read(&home.rbio, inum, F::retry_if_stale | F::may_promote, hooks).await;
    let b = ReadHome(home);
    match end {
        End::Readahead => end_reads(&b, ret, None),
        End::Single { folio, extend, r } => {
            if extend {
                end_reads(&b, ret, Some(folio));
            }
            r.complete(ret);
        }
    }
}

/// A read of @inode into a bio of @nr_vecs bvecs, set up by @set_up: read by a
/// task of its own, its first poll here, and ended as @end says.
fn spawn_read(fs: &Fs, inode: &InodeInfo, opts: c::bch_inode_opts, nr_vecs: usize,
              set_up: impl FnOnce(Pin<&mut Bio>), hooks: PagecacheRead, end: End) {
    let subvol = inode.inum().subvol as u32;
    let mut slot = TaskSlot::exclusive_bio_in(&READS, core::ptr::null_mut(), nr_vecs as u16, c::req_op_REQ_OP_READ,
                                              |b| pin_init!(ReadBio { rbio <- Rbio::new(fs, opts, subvol, None, b) }? BchError))
        .expect("a read bio's box: its pool waits for an element, and an Rbio's init can't fail");
    set_up(slot.home().bio_mut());
    let inum = inode.inum();
    unsafe { slot.start_inline(Queue::system_dfl(), move |h| read_task(h, inum, hooks, end)) };
}

pub(crate) fn readahead(mut ra: Readahead<'_>) {
    let mapping = ra.mapping();
    let inode = mapping.inode();
    let fs = inode.fs();
    let opts = get_inode(fs, &inode.inode());

    let Ok(mut folios) = Folios::take(&mut ra) else { return };

    /*
     * Besides being a general performance optimization, plugging helps with
     * avoiding btree transaction srcu warnings - submitting a bio can
     * block, and we don't want todo that with the transaction locked.
     *
     * However, plugged bios are submitted when we schedule; we ideally
     * would have our own scheduler hook to call unlock_long() before
     * scheduling.
     */
    plugged(|| {
        let Some(_add) = inode.ei_pagecache_lock.try_lock::<c::PagecacheAdd>() else {
            return folios.give_back(&mut ra);
        };

        let orig_end = folios.folios.last().map_or(0, |f| f.end_sector());
        while let Some(folio) = folios.peek() {
            let nr_vecs = folios.remaining().min(c::BIO_MAX_VECS as usize);
            folios.advance();
            let set_up = |mut b: Pin<&mut Bio>| {
                b.as_mut().set_sector(folio.sector());
                unsafe { b.add_folio(folio) };
            };

            let hooks = PagecacheRead {
                folios:    Some(NonNull::from(&mut folios)),
                mapping,
                read_from: folio.sector() + u64::from(folio.partially_uptodate()),
                orig_end,
            };
            spawn_read(fs, inode, opts, nr_vecs, set_up, hooks, End::Readahead);
        }
    });
}

/// Read one folio, synchronously; @folio is locked and stays locked.
///
/// A partial read of a checksummed or compressed extent reads the whole extent
/// anyway. With @extend, the rest of it goes into the page cache, as readahead
/// does, rather than being thrown away: without it a sequential rewrite that
/// doesn't cover whole folios reads the same extent once per folio. The caller
/// must hold pagecache_add, since new folios are added.
pub(crate) fn read_single_folio(folio: Folio, mapping: Mapping, extend: bool) -> Result<(), BchError> {
    assert!(!folio.uptodate() && !folio.dirty(), "reading a folio uptodate or dirty");

    if !folio.create_state() {
        return Err(BchError::from_raw(c::ENOMEM as i32));
    }

    let inode = mapping.inode();
    let fs = inode.fs();
    let opts = get_inode(fs, &inode.inode());
    let nr_vecs = if extend {
        (c::BIO_MAX_VECS as usize).min((fs.opts().encoded_extent_max.get() as usize).div_ceil(PAGE_SIZE) + 1)
    } else {
        1
    };

    let mut folios = Folios::new();
    let set_up = |mut b: Pin<&mut Bio>| {
        b.as_mut().set_opf(c::req_op_REQ_OP_READ | c::REQ_SYNC);
        b.as_mut().set_sector(folio.sector());
        unsafe { b.add_folio(folio) };
    };

    let hooks = PagecacheRead {
        folios:    extend.then(|| NonNull::from(&mut folios)),
        mapping,
        read_from: folio.sector() + u64::from(folio.partially_uptodate()),
        orig_end:  folio.end_sector(),
    };
    let mut slot = None;
    let (wait, r) = CompletionReturn::new(&mut slot, waiter::Sync::current());
    /*
     * The folios extend() added after @folio are locked and ours to finish,
     * as readahead's end does; @folio stays locked, it's the caller's.
     */
    let end = End::Single { folio, extend, r };
    plugged(|| spawn_read(fs, inode, opts, nr_vecs, set_up, hooks, end));
    let ret = wait.wait(Sleep::UNINTERRUPTIBLE).expect("a read's task, ended with what it read");

    ret?;
    folio.set_partially_uptodate(0);
    folio.mark_uptodate();
    Ok(())
}
