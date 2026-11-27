// SPDX-License-Identifier: GPL-2.0

/*
 * readpage(s):
 *
 * A readahead reads the folios the readahead control asked for, plus whatever
 * extend() adds to reach an extent boundary - partial reads of a checksummed
 * or compressed extent are expensive. Only that extension is speculative, and
 * only it may be abandoned.
 *
 * Abandoning it is worth doing because extend() drops btree locks to allocate
 * folios: under write load we lose the relock repeatedly to an extent that
 * keeps moving, and holding the transaction open to chase it costs more than
 * the readahead is worth.
 *
 * The complication is that reads are sector granular and the page cache is
 * folio granular, so where we stop issuing is generally not a folio boundary,
 * and an extent's bio has parts of the folios at its ends. Every folio is
 * unlocked exactly once, by its read's last count: the window's, given up as
 * it passes the folio or stops, or an extent's bio's. The folio we stop inside
 * isn't uptodate, and folio_end_read() can't express that. bch_folio.
 * partially_uptodate does.
 */

use super::folio::{FolioLocked, FolioSpan, Mapping, ReadPart, Readahead};
use super::inode::InodeInfo;
use super::pagecache;
use crate::btree::bkey::{pos, prelude::*, BkeySC};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, LoopFlow, TransAttempt};
use crate::data::{ptrs::{BkeyVPtrs, Ptrs}, read::{self, Rbio}, extents::ReadFlags};
use crate::errcode::{Errcode as err, BchError};
use crate::format::{Bpos, Btree, CompressionType, CsumType};
use crate::fs::Fs;
use crate::init::error::inum_offset_err_msg_trans;
use crate::posix::inode_opts::get_inode;
use crate::data_structures::fifo::FifoRead;
use crate::types::Fifo;
use crate::os::blkdev::plugged;
use crate::os::bio::{Bvec, BvecMempool, Holds};
use crate::util::gather::{Gather, GatherRef};
use crate::os::kernel::PAGE_SIZE;
use crate::os::module::{Quiesced, ThisModule};
use crate::util::late::{Late, ModuleInit};
use crate::snapshots::subvolume::for_each_in_subvolume_max;
use crate::util::mempool::Mempool;
use crate::util::sleep::Sleep;
use crate::util::task::{exclusive_size, waiter, TaskSlot, WaitSync};
use crate::{btree_trans, c, ratelimit};
use core::pin::Pin;
use pin_init::{pin_data, pin_init, pinned_drop, stack_pin_init};
use crate::opts::InodeOpts;
use crate::vfs::inode::PagecacheAdd;

type F = ReadFlags;
/// A single folio's read: its caller waits for each extent's
type Single = Gather<Result<(), BchError>, waiter::Sync>;
type SingleRef = GatherRef<'static, Result<(), BchError>, waiter::Sync>;

const PAGE_SECTORS: u64 = (PAGE_SIZE >> 9) as u64;

/// A read's window, from @pos to @end: the folios the readahead asked for, a
/// count on each one's read, and the new ones a read extends it by. Handed
/// out from the front, as parts of extents' bios; each folio's count is
/// given up as the window passes it, or stops inside it.
struct ReadaheadIter {
    mapping: Mapping,
    folios:  Fifo<ReadPart>,
    pos:     u64,
    end:     u64,
}

impl ReadaheadIter {
    /// From @folios' first's read_from - what of it isn't read already.
    fn of(mapping: Mapping, folios: Fifo<ReadPart>) -> Option<Self> {
        let pos = folios.peek_front().map(|f| f.sector() + u64::from(f.partially_uptodate()))?;
        let end = folios.peek_back().map(|f| f.end_sector())?;
        Some(ReadaheadIter { mapping, folios, pos, end })
    }

    /// A single folio's: @folio, for the read to extend past.
    fn new(mapping: Mapping, folio: ReadPart) -> Result<Self, BchError> {
        let mut folios = Fifo::new();
        folios.reserve(1).map_err(|_| BchError::from(err::ENOMEM_readahead_folios_reserve))?;
        let pushed = folios.push_back(folio);
        assert!(pushed.is_ok(), "a readahead window's folio, pushed into room reserved for it");
        Ok(Self::of(mapping, folios).expect("a window of a folio"))
    }

    fn take(ra: &mut Readahead<'_>) -> Result<Self, BchError> {
        let mut folios = Fifo::new();
        while let Some(folio) = ra.next() {
            if folio.create_private().is_none() || folios.reserve(1).is_err() {
                ra.unread(folio);
                break;
            }
            let pushed = folios.push_back(folio.start_read());
            assert!(pushed.is_ok(), "a readahead window's folio, pushed into room reserved for it");
        }
        Self::of(ra.mapping(), folios).ok_or_else(|| BchError::from(err::ENOMEM_readahead_take))
    }

    /// The window extended by a new folio at its end, of up to @sectors: if
    /// the page cache hasn't one there, and it can be made.
    fn extend(&mut self, t: &TransAttempt<'_, '_>, sectors: u32) -> Option<()> {
        let mapping = self.mapping;
        if u64::from(sectors) < PAGE_SECTORS << mapping.min_folio_order() {
            return None;
        }

        let folio_offset = self.end / PAGE_SECTORS;
        let order = (u64::from(sectors) / PAGE_SECTORS).ilog2()
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

        let enomem = || BchError::from(err::ENOMEM_readahead_extend);
        let gfp = mapping.readahead_gfp();
        t.allocate_dropping_locks_norelock(|| self.folios.reserve(1).map_err(|_| enomem())).ok()?;
        let mut folio = t.allocate_dropping_locks_norelock(|| mapping.alloc_folio(gfp, order).ok_or_else(enomem)).ok()?;
        t.allocate_dropping_locks_norelock(|| folio.create_private().map(drop).ok_or_else(enomem)).ok()?;
        t.allocate_dropping_locks_norelock(|| mapping.add_folio(&mut folio, folio_offset, c::GFP_KERNEL)).ok()?;

        let folio = folio.into_cached().start_read();
        self.end = folio.end_sector();
        let pushed = self.folios.push_back(folio);
        assert!(pushed.is_ok(), "a readahead window's folio, pushed into room reserved for it");
        Some(())
    }

    /// @to, or short of it, at the end of as many folios from @pos as a bio
    /// has room for.
    fn bio_end(&self, to: u64) -> u64 {
        let to = to.min(self.end);
        self.folios.iter().nth(c::BIO_MAX_VECS as usize - 1).map_or(to, |f| to.min(f.end_sector()))
    }

    /// How many folios [@pos, @to) is parts of.
    fn nr_parts(&self, to: u64) -> usize {
        self.folios.iter().take_while(|f| f.sector() < to).count()
    }

    /// [@pos, @to) of its folios, a part of each, and a count on its read:
    /// nothing's taken till advance().
    fn parts(&self, to: u64) -> impl Iterator<Item = Bvec<ReadPart>> + '_ {
        let pos = self.pos;
        self.folios.iter().take_while(move |f| f.sector() < to).map(move |f| {
            let FolioSpan { offset, len, .. } = FolioSpan::new(f, pos << 9, to << 9);
            Bvec::new(f.share(), offset, len)
        })
    }

    /// On to @to: each folio it passes handed out, all of it.
    fn advance(&mut self, to: u64) {
        while self.folios.peek_front().is_some_and(|f| f.end_sector() <= to) {
            if let Some(f) = self.folios.pop_front() {
                f.covered();
            }
        }
        self.pos = to;
    }
}

impl Drop for ReadaheadIter {
    /* the folio it stopped inside: read to @pos, if its extents' reads are */
    fn drop(&mut self) {
        if let Some(f) = self.folios.peek_front().filter(|f| self.pos > f.sector()) {
            f.set_partially_uptodate((self.pos - f.sector()) as u16);
        }
    }
}

/// An extent's read into the page cache: the home of the task that reads it,
/// its bio parts of the folios it reads into.
#[pin_data(PinnedDrop)]
#[repr(C)]
struct ReadBio {
    #[pin]
    rbio: Rbio<Holds<ReadPart>>,
    single: Option<SingleRef>,
}
crate::has_bio!(ReadBio, rbio.bio, owns Holds<ReadPart>);

#[pinned_drop]
impl PinnedDrop for ReadBio {
    /* its parts given back - failed, if its read did - before its caller's told */
    fn drop(self: Pin<&mut Self>) {
        let this = self.get_mut();
        let ret = this.rbio.ret();
        let bio = this.rbio.bio.get_mut();
        if ret.is_err() {
            for v in bio.vecs() {
                v.folio().read_failed();
            }
        }
        bio.release();
        if let Some(r) = this.single.take() {
            r.put(ret);
        }
    }
}

/// A read's box is freed when its task is done, after the read - when the
/// filesystem may be gone: so from a pool of the module's.
static READS: Late<Mempool, ModuleInit> = ::pin_init::zeroed();
static READ_BVECS: Late<BvecMempool, ModuleInit> = ::pin_init::zeroed();

pub fn pools_init(m: &ThisModule) -> Result<(), BchError> {
    let size = exclusive_size::<ReadBio, read::ExtentFut<'static>, &'static Mempool>();
    READS.init(m, || Mempool::new(1, size))?;
    READ_BVECS.init(m, || BvecMempool::new(1))
}

pub fn pools_exit(q: &Quiesced<'_>) {
    READ_BVECS.exit(q);
    READS.exit(q)
}

/// What a read into the page cache reads: an inode, from its window.
struct PagecacheRead<'a> {
    fs:       &'a Fs,
    inode:    &'a InodeInfo,
    opts:     InodeOpts,
    /// The readahead window: past this is the speculative extension
    orig_end: u64,
    extend:   bool,
    single:   Option<&'a Single>,
}

/// Whether reading part of @k reads all of it.
fn encoded(fs: &Fs, k: BkeySC<'_>) -> bool {
    k.arm::<Ptrs>().is_some_and(|kp| kp.crcs(fs).any(|crc| crc.csum_type != CsumType::none ||
                                                         crc.compression_type != CompressionType::none))
}

/// The extent at @read_pos, of @k, read into @iter from where it is: a bio of
/// the parts of its folios, given to a task of its own. Nothing's taken from
/// @iter unless the task's started.
fn read_extent(t: &TransAttempt<'_, '_>, w: &PagecacheRead<'_>, iter: &mut ReadaheadIter, read_pos: Bpos,
               k: BkeySC<'_>) -> Result<(), BchError> {
    let fs = w.fs;
    let e = read::extent_of(t, read_pos, k)?;
    let end = e.read_pos.offset + u64::from(e.sectors);

    if w.extend && end > iter.end && encoded(fs, e.k) {
        while iter.end < end && iter.folios.len() < c::BIO_MAX_VECS as usize &&
              iter.extend(t, (end - iter.end) as u32).is_some() {}
    }

    let to = iter.bio_end(end);
    let (subvol, nr_vecs) = (w.inode.inum().subvol as u32, iter.nr_parts(to) as u16);
    /* not waited for with btree locks held: its pool's wait is for an extent's read in flight */
    let mut slot = t.allocate_dropping_locks_norelock(|| {
        TaskSlot::exclusive_bio_in(&*READS, &READ_BVECS, nr_vecs, c::req_op_REQ_OP_READ,
                                   |b| pin_init!(ReadBio {
            rbio   <- Rbio::new(w.opts, subvol, None, b.init()),
            single: w.single.map(Single::get_pinned),
        }? BchError))
    })?;
    t.relock()?;
    {
        let bio = slot.home().get_mut().rbio.bio.get_mut();
        for v in iter.parts(to) {
            bio.push(v).expect("a read bio's bvecs, room made for its parts");
        }
        bio.set_sector(iter.pos);
        if w.single.is_some() {
            bio.set_opf(c::req_op_REQ_OP_READ | c::REQ_SYNC);
        }
        pagecache::folios_state_set(fs, bio.vecs().iter().map(|v| v.folio()), iter.pos..to, e.k);
    }

    let mut restart = Ok(());
    let make = read::extent_make(fs, t, e, F::retry_if_stale | F::may_promote, &mut restart);
    read::start_task(t, false, |at| unsafe {
        slot.start_unchecked(at, fs.tasks(), move |h: Pin<&'static mut ReadBio>| make(h.into_ref().get_ref().rbio.erased()))
    });
    restart?;
    iter.advance(to);
    Ok(())
}

/// @iter read, an extent's bio at a time, each read by a task of its own -
/// started here, inline, and not waited for.
fn read_pagecache(w: &PagecacheRead<'_>, iter: &mut ReadaheadIter) -> Result<(), BchError> {
    let inum = w.inode.inum();
    let trans = btree_trans!(w.fs);
    let mut btree_iter = BtreeIter::new(&trans, Btree::extents, pos(inum.inum, iter.pos), BtreeIterFlags::SLOTS);
    let ret = for_each_in_subvolume_max(&trans, &mut btree_iter, pos(inum.inum, u64::MAX), inum.subvol as u32,
                                        BtreeIterFlags::SLOTS, |t, btree_iter, k| {
        match read_extent(t, w, iter, btree_iter.pos(), k) {
            /* everything asked for is issued; the rest isn't worth a retry */
            Err(e) if e.matches(err::transaction_restart) && iter.pos >= w.orig_end => {
                t.restarts_handled();
                Ok(LoopFlow::Break(()))
            }
            Err(e) => Err(e),
            Ok(()) if iter.pos >= iter.end => Ok(LoopFlow::Break(())),
            Ok(()) => Ok(LoopFlow::ContinueFrom(pos(inum.inum, iter.pos))),
        }
    });

    if let Err(e) = ret {
        let mut m = ratelimit!(w.fs.log()).msg();
        write!(m, "data read error at ");
        inum_offset_err_msg_trans(&trans, &mut m, inum.subvol as u32, pos(inum.inum, iter.pos));
        write!(m, ": data read error: {e}");
    }
    ret
}

pub(crate) fn readahead(mut ra: Readahead<'_>) {
    let mapping = ra.mapping();
    let inode = mapping.inode();
    let fs = inode.fs();
    let opts = get_inode(fs, &inode.inode());

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
        /* the folios not taken, read_pages() gives up */
        let Some(_add) = inode.ei_pagecache_lock.try_lock::<PagecacheAdd>() else { return };
        let Ok(mut iter) = ReadaheadIter::take(&mut ra) else { return };

        let w = PagecacheRead { fs, inode, opts, orig_end: iter.end, extend: true, single: None };
        let _ = read_pagecache(&w, &mut iter);
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
        return Err(BchError::from(c::ENOMEM));
    }

    let inode = mapping.inode();
    let fs = inode.fs();
    let opts = get_inode(fs, &inode.inode());

    stack_pin_init!(let single = Single::new());
    let single: &Single = &single;
    /*
     * The folios extend() adds after @folio are the read's to end, as
     * readahead's are; @folio stays locked, it's the caller's, and we wait for
     * every extent's read it's in.
     */
    let walked = ReadaheadIter::new(mapping, unsafe { ReadPart::callers(folio) }).and_then(|mut iter| {
        let w = PagecacheRead { fs, inode, opts, orig_end: folio.end_sector(), extend, single: Some(single) };
        plugged(|| read_pagecache(&w, &mut iter))
    });
    let ret = single.wait_sync(Sleep::UNINTERRUPTIBLE);

    walked.and(ret)?;
    folio.set_partially_uptodate(0);
    folio.mark_uptodate();
    Ok(())
}
