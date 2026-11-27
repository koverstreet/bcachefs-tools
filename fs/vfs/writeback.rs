// SPDX-License-Identifier: GPL-2.0

//! Buffered writeback: buffered.c's writepages. A writeback walk's dirty
//! sectors, gathered into ios - one bio's worth of contiguous sectors at one
//! replica count each. Each io is the home of a task, in one
//! allocation with the task and its bvecs, from a pool of the module's - its
//! folios added before it's started, then its write awaited, and its folios'
//! writeback ended.

use crate::btree::bkey::pos;
use crate::c;
use crate::alloc::{foreground::{OPEN_BUCKETS_COUNT, Watermark}, write_point::WritePointSpecifier};
use crate::opts::InodeOpts;
use crate::util::time_stats::TimeStat;
use crate::vfs::inode::InodeInfoFlag;
use crate::vfs::pagecache::{FolioSector, FolioSectorState as SectorState};
use crate::data::write::{write, WriteBio, WriteFor, WriteOp, WriteFlags as w};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::posix::inode_opts;
use crate::locking::MemallocFlags;
use crate::util::alloc::Box;
use crate::os::blkdev::plugged;
use crate::os::bio::{Bio, BioSubmit, Bvec, BvecMempool, Holds};
use crate::util::mempool::Mempool;
use crate::util::task::{exclusive_size, Exclusive, Resume, Spawn, TaskSlot};
use crate::os::kernel::{local_clock, unlikely};
use crate::os::module::{Quiesced, ThisModule};
use crate::os::workqueue::Queue;
use crate::util::sleep::Sleep;
use crate::os::ops::from_raw;
use crate::os::folio::Wbc;
use super::folio::{Folio, FolioGuard, FolioSpan, Mapping};
use crate::vfs::folio::Writeback;
use core::pin::Pin;
use cstruct_macros::in_place;
use pin_init::{pin_data, pin_init};

#[pin_data]
struct Io {
    mapping: Mapping,
    #[pin]
    op:      WriteOp,
    #[pin]
    wbio:    Bio<Holds<Writeback>>,
}
crate::has_bio!(Io, wbio, owns Holds<Writeback>);

type Pool = &'static Mempool;
/// An io, being filled: its task not started.
type IoSlot = TaskSlot<'static, Io, WritepageFut<'static>, Pool, Exclusive>;

static WRITEPAGES: Mempool = ::pin_init::zeroed();
static WRITEPAGE_BVECS: BvecMempool = ::pin_init::zeroed();

pub fn pools_init(m: &ThisModule) -> Result<(), BchError> {
    Mempool::init_static(&WRITEPAGES, Mempool::new(4, exclusive_size::<Io, WritepageFut<'static>, Pool>()), m)?;
    BvecMempool::init_static(&WRITEPAGE_BVECS, 4, m)
}

pub fn pools_exit(q: &Quiesced<'_>) {
    WRITEPAGE_BVECS.exit(q);
    WRITEPAGES.exit(q);
}

struct WritepageState {
    io:   Option<IoSlot>,
    opts: InodeOpts,
}

/// Its io, not started: ours to fill.
fn io_of(slot: &mut IoSlot) -> &mut Io {
    slot.home().get_mut()
}

impl BioSubmit for Io {
    fn bio(&self) -> &Bio {
        &self.wbio
    }

    fn bio_mut(&mut self) -> &mut Bio {
        &mut self.wbio
    }
}

fn bio_full(bio: &Bio, len: u32) -> bool {
    bio.nr_vecs() >= bio.max_vecs() || bio.range().len() > u32::MAX - len
}

/// Every sector of each of the io's folios left with no replicas - after
/// @f, on the folio.
fn folios_clear_replicas(io: &Io, f: impl Fn(&Folio)) {
    for v in io.wbio.vecs() {
        let folio = v.folio();
        f(folio);

        let mut s = folio.lock_state();
        for x in s.sectors() {
            x.bits.set_nr_replicas(0);
        }
    }
}

fn writepage_io_done(io: &mut Io) {
    let mapping = io.mapping;
    let inode = mapping.inode();

    if io.op.error.is_some() {
        inode.ei_flags.set(InodeInfoFlag::EI_INODE_ERROR, true);

        folios_clear_replicas(io, |folio| {
            folio.mapping().expect("a folio in writeback with no mapping").set_error(-(c::EIO as i32))
        });
    }

    if io.op.has(w::wrote_data_inline) {
        folios_clear_replicas(io, |_| {});
    }

    /*
     * racing with fallocate can cause us to add fewer sectors than
     * expected - but we shouldn't add more sectors than expected:
     */
    crate::warn_on_once!(io.op.i_sectors_delta > 0);

    /*
     * (error (due to going RO) halfway through a page can screw that up
     * slightly)
     * XXX wtf?
       BUG_ON(io->op.op.i_sectors_delta >= PAGE_SECTORS);
     */

    /*
     * The writeback flag is effectively our ref on the inode -
     * fixup i_blocks before calling folio_end_writeback:
     */
    inode.i_sectors_acct(None, io.op.i_sectors_delta);

    io.wbio.release();
}

#[in_place(WritepageFut)]
async fn writepage_task<'h>(io: Pin<&'h mut Io>) {
    let io = io.get_mut();
    let mapping = io.mapping;
    let inode = mapping.inode();
    write(&mut io.op, WriteBio::Bio(&mut io.wbio), WriteFor::Inode(&inode.ei_devs_need_flush),
          Resume::queue(Queue::system_dfl()))
        .await;
    writepage_io_done(io);
}

fn writepage_do_io(w: &mut WritepageState) {
    let mut slot = w.io.take().expect("writepage_do_io() with no io");
    let fs = io_of(&mut slot).op.fs();

    unsafe { slot.start_unchecked(Spawn::Inline, fs.tasks(), writepage_task) };
}

/*
 * Get a writepage io and add @page to it - appending to an existing one if
 * possible, else allocating a new one:
 */
fn writepage_io_alloc(fs: &Fs, wbc: &Wbc, w: &mut WritepageState, mapping: Mapping,
                      sector: u64, nr_replicas: u32, res_nr_replicas: u32) {
    let opts = w.opts;
    let mut slot = IoSlot::exclusive_bio_in(&WRITEPAGES, &WRITEPAGE_BVECS, c::BIO_MAX_VECS as u16,
                                            c::req_op_REQ_OP_WRITE, |b| {
        pin_init!(Io {
            mapping,
            op   <- WriteOp::init(fs, opts),
            wbio <- b.init::<BchError, _>(),
        }? BchError)
    }).expect("a writepage io's box: its pool waits for an element, and an op's init can't fail");

    let inode = mapping.inode();
    let io = io_of(&mut slot);
    let op = &mut io.op;
    op.target          = opts.foreground_target;
    op.bits.set_nr_replicas(nr_replicas as u8);
    op.res.nr_replicas.set(res_nr_replicas);
    op.write_point     = WritePointSpecifier::hashed(inode.ei_last_dirtied.get() as usize);
    op.subvol          = inode.inum().subvol as u32;
    op.pos             = pos(inode.inum().inum, sector);
    io.wbio.set_sector(sector);
    io.wbio.set_opf(wbc.write_flags());

    w.io = Some(slot);
}

fn throttle_writes(fs: &Fs) {
    let reserved = OPEN_BUCKETS_COUNT -
        (OPEN_BUCKETS_COUNT - crate::alloc::foreground::open_buckets_reserved(Watermark::normal)) / 2;

    let mut start = 0;
    loop {
        let open_buckets_seen = fs.allocator.open_buckets_events.snapshot();
        let journal_seen = fs.journal.async_wait.snapshot();
        let open_buckets_low = unlikely(u32::from(fs.allocator.freelist_lock.open_buckets_nr_free.get()) <= reserved);
        let journal_low = (Watermark::normal as u32) < fs.journal.watermark() as u32 && fs.journal.error().is_ok();
        if !open_buckets_low && !journal_low {
            break;
        }

        if start == 0 {
            start = local_clock();
        }

        let Ok(()) = if open_buckets_low {
            fs.allocator.open_buckets_events.wait(open_buckets_seen, Sleep::UNINTERRUPTIBLE)
        } else {
            fs.journal.async_wait.wait(journal_seen, Sleep::UNINTERRUPTIBLE)
        };
    }

    if start != 0 {
        fs.times.get(TimeStat::blocked_writeback_throttle as usize).update(start);
    }
}

fn writepage(folio: FolioGuard<'_>, wbc: &Wbc, w: &mut WritepageState, tmp: &mut [FolioSector]) -> i32 {
    let mapping = folio.mapping().expect("a folio in writeback with no mapping");
    let inode = mapping.inode();
    let fs = inode.fs();
    let mut nr_replicas_this_write = u32::MAX;
    let i_size = inode.v.size();

    debug_assert!(folio.uptodate(), "writing back a folio that isn't uptodate");

    'do_io: {
        /* Is the folio fully inside i_size? */
        if folio.end_pos() <= i_size {
            break 'do_io;
        }

        /* Is the folio fully outside i_size? (truncate in progress) */
        if folio.pos() >= i_size {
            inode.undirty_folio(FolioSpan::whole(&folio));
            return 0;
        }

        /*
         * The folio straddles i_size.  It must be zeroed out on each and every
         * writepage invocation because it may be mmapped.  "A file is mapped
         * in multiples of the folio size.  For a file that is not a multiple of
         * the  folio size, the remaining memory is zeroed when mapped, and
         * writes to that region are not written out to the file."
         */
        let f_offset = (i_size - folio.pos()) as usize;
        folio.zero_segment(f_offset, folio.size());

        inode.undirty_folio(FolioSpan::new(&folio, i_size, folio.end_pos()));
    }

    let f_sectors = folio.nr_sectors();

    /*
     * Things get really hairy with errors during writeback:
     */
    folio.reserve_writeback(inode).expect("writeback's disk reservation can't fail");

    /* Before unlocking the page, get copy of reservations: */
    let tmp = &mut tmp[..f_sectors];
    let res_nr_replicas = {
        let mut st = folio.lock_state();

        tmp.copy_from_slice(st.sectors());

        for x in st.sectors().iter().filter(|x| x.has_data()) {
            nr_replicas_this_write = nr_replicas_this_write.min(x.replicas());
        }

        for x in st.sectors().iter_mut().filter(|x| x.has_data()) {
            x.bits.set_nr_replicas(if w.opts.compression != 0 { 0 } else { nr_replicas_this_write as u8 });
            x.bits.set_replicas_reserved(0);
            x.set_state(SectorState::allocated);
        }

        st.reserved_degraded = false;
        u32::from(st.replicas_reserved_at)
    };

    let folio = folio.start_writeback();

    let mut offset = 0;
    loop {
        let (mut sectors, mut dirty_sectors, mut reserved_sectors) = (0, 0u32, 0u64);

        while offset < f_sectors && !tmp[offset].has_data() {
            offset += 1;
        }

        if offset == f_sectors {
            break;
        }

        while offset + sectors < f_sectors && tmp[offset + sectors].has_data() {
            reserved_sectors += u64::from(tmp[offset + sectors].bits.replicas_reserved());
            dirty_sectors += u32::from(tmp[offset + sectors].state() == SectorState::dirty);
            sectors += 1;
        }
        assert!(sectors != 0, "a run of no dirty sectors");

        let sector = folio.sector() + offset as u64;

        /*
         * One io's reservation is charged at one count, so a folio in
         * another slot starts a new io, as a differing width does.
         */
        if let Some(slot) = w.io.as_mut() {
            let io = io_of(slot);
            let bio = io.wbio.raw();
            if u32::from(io.op.bits.nr_replicas()) != nr_replicas_this_write ||
               io.op.res.nr_replicas.get() != res_nr_replicas ||
               bio_full(&io.wbio, (sectors << 9) as u32) ||
               bio.bi_iter.bi_sector + u64::from(bio.bi_iter.bi_size >> 9) != sector {
                writepage_do_io(w);
            }
        }

        if w.io.is_none() {
            writepage_io_alloc(fs, wbc, w, mapping, sector, nr_replicas_this_write, res_nr_replicas);
        }

        let io = io_of(w.io.as_mut().expect("an io, just made"));
        assert!(io.mapping == mapping, "a writepage io of another inode");
        io.wbio.push(Bvec::new(folio.share(), offset << 9, sectors << 9))
            .expect("a writepage io's bvecs: room for BIO_MAX_VECS, and bio_full() checked");

        io.op.res.sectors.set(io.op.res.sectors.get() + reserved_sectors);
        io.op.i_sectors_delta -= i64::from(dirty_sectors);
        io.op.new_i_size = i_size;

        offset += sectors;
    }

    drop(folio);
    0
}

pub(super) fn writepages(mapping: Mapping, wbc: Wbc) -> i32 {
    let inode = mapping.inode();
    let fs = inode.fs();
    let pool = &fs.vfs.writepage_buf_pool;
    let tmp = {
        let _noio = MemallocFlags::noio();
        Box::<[FolioSector], _>::try_new_zeroed_slice_in(
            pool.element_size() / size_of::<FolioSector>(), &**pool)
    }.expect("a pool's allocation: it waits for an element");
    // zeroed is a FolioSector
    let mut tmp = unsafe { tmp.assume_init() };
    let mut w = WritepageState {
        io:   None,
        opts: inode_opts::get_inode(fs, &inode.inode()),
    };

    let ret = plugged(|| {
        let mut ret = 0;

        loop {
            throttle_writes(fs);
            let Some(folio) = wbc.next(mapping, &mut ret) else { break };
            ret = writepage(folio, &wbc, &mut w, &mut tmp);
        }

        if w.io.is_some() {
            writepage_do_io(&mut w);
        }
        ret
    });

    drop(tmp);

    BchError::from_ret(ret.into()).map_or(ret, |e| e.class())
}
