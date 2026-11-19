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

use super::folio::{Folio, FolioGuard, FolioLocked, Mapping, Readahead};
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
use crate::util::alloc::{KVVec, Kmalloc, PushError};
use crate::os::blkdev::plugged;
use crate::util::bio::{bvec_offset, inline_vecs, Bio, BioRange, HasBio, Lent};
use crate::os::kernel::PAGE_SIZE;
use crate::os::module::{Quiesced, ThisModule};
use crate::util::mempool::Mempool;
use crate::util::sleep::Sleep;
use crate::util::task::{exclusive_size, waiter, CompletionReturn, DoneWith, Return, Spawn, TaskSlot};
use crate::os::workqueue::Queue;
use core::pin::Pin;
use core::ptr::NonNull;
use cstruct_macros::in_place;
use pin_init::{pin_data, pin_init};

type F = c::ReadFlags;

const PAGE_SECTORS: u64 = (PAGE_SIZE >> 9) as u64;

/// The folios a readahead asked for, each with its bcachefs state, last
/// first: read from the end, or given back.
struct Folios(KVVec<FolioGuard<'static>>);

impl Folios {
    fn new() -> Self {
        Folios(KVVec::new())
    }

    fn take(ra: &mut Readahead<'_>) -> Result<Self, BchError> {
        let mut f = Folios::new();
        while let Some(folio) = ra.next() {
            let ok = folio.create_private().is_some() && f.0.reserve(1).is_ok();
            let pushed = if ok { f.0.push_within_capacity(folio) } else { Err(PushError(folio)) };
            if let Err(PushError(folio)) = pushed {
                ra.unread(folio);
                if f.0.is_empty() {
                    return Err(BchError::from(c::ENOMEM));
                }
                break;
            }
        }
        f.0.reverse();
        Ok(f)
    }

    fn next(&mut self) -> Option<FolioGuard<'static>> {
        self.0.pop()
    }

    fn remaining(&self) -> usize {
        self.0.len()
    }

    /// Back to front, as unread() wants them.
    fn give_back(&mut self, ra: &mut Readahead<'_>) {
        for folio in self.0.drain_all() {
            ra.unread(folio);
        }
    }
}

/// A read's folios, its bio's bvecs: a single folio's read's first, its lock
/// the caller's, then those whose locks are the read's - each given up as the
/// read ends, or as truncate() abandons it. Folios go into the bio only
/// through this, and their locks are given up only here: so each bvec is a
/// folio locked for the read, and the read's task, holding this, holds the
/// locks. Nothing's kept but how many: the bvecs are the folios.
///
/// XXX: the bio should be made of its folios - Bio<B>, typed by its buffers,
/// walked as them - and the casts here go.
struct ReadFolios {
    /// The first is the caller's
    callers: bool,
    /// How many after it are the read's
    locked:  u16,
}

impl ReadFolios {
    /// A read's, @folio its first, and its lock the read's.
    fn starting_with(bio: Pin<&mut Bio>, folio: FolioGuard<'static>) -> Self {
        let mut f = ReadFolios { callers: false, locked: 0 };
        f.push(bio, folio);
        f
    }

    /// A single folio's read's, @folio its first, its lock the caller's, who
    /// waits for the read.
    fn callers(bio: Pin<&mut Bio>, folio: &FolioLocked) -> Self {
        unsafe { bio.add_folio(folio) };
        ReadFolios { callers: true, locked: 0 }
    }

    /// The folio in each bvec of @range, of this read's bio.
    fn folios<'b>(&'b self, range: BioRange<'b>) -> impl Iterator<Item = &'b Folio> {
        range.folios().map(|fv| unsafe { Folio::from_raw(fv.folio) })
    }

    /// @folio into @bio, after the rest: its lock the read's.
    fn push(&mut self, bio: Pin<&mut Bio>, folio: FolioGuard<'static>) {
        unsafe { bio.add_folio(&folio) };
        folio.keep_locked();
        self.locked += 1;
    }

    /// The read's folios from bvec @from on, their locks given back: the
    /// caller's is never the read's.
    fn take<'b>(&mut self, bio: &'b Bio, from: usize) -> impl Iterator<Item = FolioGuard<'static>> + 'b {
        let first = self.callers as usize;
        let from = from.max(first);
        self.locked = (from - first) as u16;
        bio.range_all().folios().skip(from).map(|fv| unsafe { FolioGuard::adopt(fv.folio) })
    }

    /// Each folio from bvec @keep on, never to be read: its read abandoned,
    /// and @bio truncated to the rest.
    fn truncate(&mut self, mut bio: Lent<'_>, keep: usize) {
        for folio in self.take(&bio, keep) {
            folio.end_read(false);
        }
        bio.bio_mut().truncate_vecs(keep as u16);
    }

    /// The read's done: each folio whose lock is the read's unlocked, uptodate
    /// if the read read all of it.
    fn end(mut self, bio: &Bio, ret: &Result<(), BchError>) {
        /* a stale partially_uptodate would spin do_read_cache_folio() forever: */
        let partial = self.folios(bio.range_all()).last().is_some_and(|f| f.partially_uptodate() != 0);
        let n = self.locked as usize;
        for (i, folio) in self.take(bio, 0).enumerate() {
            folio.end_read(ret.is_ok() && !(i + 1 == n && partial));
        }
    }
}

/// What a read into the page cache reads, and may extend into.
struct Window {
    /// The readahead's folios to extend into - none but new ones, for a
    /// single folio's read that's extended - or None, not extended
    batch:     Option<NonNull<Folios>>,
    mapping:   Mapping,
    read_from: u64,
    /// The readahead window: past this is the speculative extension
    orig_end:  u64,
}

// The batch is on the stack of the read's caller: the hooks are called only
// in read()'s walk, before its first await, which runs inline in the caller.
unsafe impl Send for Window {}

/// What a read into the page cache does besides read::read()'s.
struct PagecacheRead<'r> {
    window: Window,
    read:   &'r mut ReadFolios,
}

impl Window {
    fn new_folio(&self, t: &TransAttempt<'_, '_>, end_sector: u64, sectors_remaining: u32)
        -> Option<FolioGuard<'static>>
    {
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
        t.allocate_dropping_locks_norelock(|| folio.create_private().map(drop).ok_or_else(enomem)).ok()?;
        t.allocate_dropping_locks_norelock(|| mapping.add_folio(&mut folio, folio_offset, c::GFP_KERNEL)).ok()?;
        Some(folio.into_cached())
    }
}

impl ReadHooks for PagecacheRead<'_> {
    fn read_from(&self) -> u64 {
        self.window.read_from
    }

    fn extend(&mut self, t: &TransAttempt<'_, '_>, read: &Rbio, k: BkeySC<'_>, sectors: u32)
        -> Result<(), BchError>
    {
        let Some(folios) = self.window.batch else { return Ok(()) };
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

            let folio = match folios.next() {
                Some(folio) => folio,
                None if get_more => match self.window.new_folio(t, end_sector, sectors - bio_sectors) {
                    Some(folio) => folio,
                    None => break,
                },
                None => break,
            };

            assert_eq!(folio.sector(), end_sector, "a folio added to a read bio, not at its end");
            self.read.push(bio.bio_mut(), folio);
        }
        drop(bio);

        t.relock()
    }

    fn extent(&mut self, read: &Rbio, range: BioRange<'_>, k: BkeySC<'_>) {
        let start = range.pos().bi_sector;
        let sectors = start..start + u64::from(range.len() >> 9);
        pagecache::folios_state_set(read.fs(), self.read.folios(range), sectors, k);
    }

    /* everything asked for is issued; the rest isn't worth a retry */
    fn give_up(&mut self, read: &Rbio) -> bool {
        let bio = read.bio.lend();
        let iter = bio.raw().bi_iter;
        if self.window.batch.is_none() || iter.bi_sector < self.window.orig_end {
            return false;
        }

        let mut keep = iter.bi_idx as usize;
        let partial = bvec_offset(&iter);
        if partial != 0 {
            let folio = self.read.folios(bio.range()).next().expect("a partial bvec's folio");
            folio.set_partially_uptodate((partial >> 9) as u16);
            keep += 1;
        }
        self.read.truncate(bio, keep);
        true
    }
}

/// A read into the page cache - readahead's, or a single folio's: the home of
/// the task that reads it, its bvecs inline after the box.
#[pin_data]
#[repr(C)]
struct ReadBio {
    #[pin]
    rbio: Rbio,
}
crate::has_bio!(ReadBio, rbio.bio);

/// A read's box is freed when its task is done, after the read - when the
/// filesystem may be gone: so from a pool of the module's.
static READS: Mempool<Kmalloc> = ::pin_init::zeroed();

/// At module init: elements for the most bvecs a bio has.
pub fn pools_init(m: &ThisModule) -> Result<(), BchError> {
    let size = exclusive_size::<ReadBio, ReadFut<'static>, &'static Mempool<Kmalloc>>(
        inline_vecs(c::BIO_MAX_VECS as u16));
    Mempool::init_static(&READS, Mempool::kmalloc(1, size), m)
}

pub fn pools_exit(q: &Quiesced<'_>) {
    READS.exit(q)
}

/// Whom a read's done for: readahead, or a single folio's caller, waiting.
enum End {
    Readahead,
    Single(Return<Result<(), BchError>, waiter::Sync>),
}

#[in_place(ReadFut)]
async fn read_task<'h>(home: Pin<&'h mut ReadBio>, inum: c::subvol_inum, mut folios: ReadFolios,
                       window: Window, end: End) {
    let hooks = PagecacheRead { window, read: &mut folios };
    let ret = read::read(&home.rbio, inum, F::retry_if_stale | F::may_promote, hooks).await;
    folios.end(&home.rbio.bio.lend(), &ret);
    if let End::Single(r) = end {
        r.complete(ret);
    }
}

/// A read of @inode into a bio of @nr_vecs bvecs, set up by @set_up, with
/// its first folio: read by a task of its own, its first poll here.
fn spawn_read(fs: &Fs, inode: &InodeInfo, opts: c::bch_inode_opts, nr_vecs: usize,
              set_up: impl FnOnce(Pin<&mut Bio>) -> ReadFolios, window: Window, end: End) {
    let subvol = inode.inum().subvol as u32;
    let mut slot = TaskSlot::exclusive_bio_in(&READS, core::ptr::null_mut(), nr_vecs as u16, c::req_op_REQ_OP_READ,
                                              |b| pin_init!(ReadBio { rbio <- Rbio::new(fs, opts, subvol, None, b) }? BchError))
        .expect("a read bio's box: its pool waits for an element, and an Rbio's init can't fail");
    let folios = set_up(slot.home().bio_mut());
    let inum = inode.inum();
    unsafe { slot.start(Spawn::Inline, fs.tasks(), move |h| read_task(h, inum, folios, window, end)) };
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

        let orig_end = folios.0.first().map_or(0, |f| f.end_sector());
        while let Some(folio) = folios.next() {
            let nr_vecs = (folios.remaining() + 1).min(c::BIO_MAX_VECS as usize);
            let sector = folio.sector();
            let window = Window {
                batch:     Some(NonNull::from(&mut folios)),
                mapping,
                read_from: sector + u64::from(folio.partially_uptodate()),
                orig_end,
            };
            let set_up = |mut b: Pin<&mut Bio>| {
                b.as_mut().set_sector(sector);
                ReadFolios::starting_with(b, folio)
            };
            spawn_read(fs, inode, opts, nr_vecs, set_up, window, End::Readahead);
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
pub(crate) fn read_single_folio(folio: &FolioLocked, mapping: Mapping, extend: bool) -> Result<(), BchError> {
    assert!(!folio.uptodate() && !folio.dirty(), "reading a folio uptodate or dirty");

    if folio.create_private().is_none() {
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
        ReadFolios::callers(b, folio)
    };

    let window = Window {
        batch:     extend.then(|| NonNull::from(&mut folios)),
        mapping,
        read_from: folio.sector() + u64::from(folio.partially_uptodate()),
        orig_end:  folio.end_sector(),
    };
    let mut slot = DoneWith::new();
    let (wait, r) = CompletionReturn::new(&mut slot, waiter::Sync::current());
    /*
     * The folios extend() added after @folio are locked and ours to finish,
     * as readahead's end does; @folio stays locked, it's the caller's.
     */
    plugged(|| spawn_read(fs, inode, opts, nr_vecs, set_up, window, End::Single(r)));
    let ret = wait.wait(Sleep::UNINTERRUPTIBLE).expect("a read's task, ended with what it read");

    ret?;
    folio.set_partially_uptodate(0);
    folio.mark_uptodate();
    Ok(())
}
