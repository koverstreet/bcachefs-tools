// SPDX-License-Identifier: GPL-2.0

//! Buffered writeback: buffered.c's writepages. A writeback walk's dirty
//! sectors, gathered into bch_writepage_ios - one bio's worth of contiguous
//! sectors at one replica count each. Each io is the home of a task, in one
//! allocation with the task and its bvecs, from a pool of the module's - its
//! folios added before it's started, then its write awaited, and its folios'
//! writeback ended.

use crate::btree::bkey::pos;
use crate::c;
use crate::data::write::{write, WriteFor};
use crate::data::write_replicas::wbio_in_place;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::inode_opts;
use crate::locking::MemallocFlags;
use crate::util::alloc::{Box, Kmalloc};
use crate::os::blkdev::plugged;
use crate::os::bio::{Bio, BioOwner, BioSubmit, BvecMempool, Data};
use crate::util::mempool::Mempool;
use crate::util::task::{exclusive_size, Exclusive, Resume, Spawn, TaskSlot};
use crate::os::kernel::{local_clock, unlikely, PAGE_SIZE};
use crate::os::module::{Quiesced, ThisModule};
use crate::os::workqueue::Queue;
use crate::util::sleep::Sleep;
use crate::os::ops::from_raw;
use crate::os::folio::Wbc;
use super::folio::{Folio, FolioGuard, FolioSpan, Mapping};
use crate::vfs::folio::Writeback;
use crate::vfs::inode::InodeInfo;
use c::FolioSectorState as SectorState;
use c::WriteFlags as w;
use core::ffi::c_int;
use core::pin::Pin;
use cstruct_macros::in_place;
use pin_init::pin_init;

type Io = c::bch_writepage_io;
type Pool = &'static Mempool<Kmalloc>;
/// An io, being filled: its task not started.
type IoSlot = TaskSlot<'static, Io, fn(Pin<&'static mut Io>) -> WritepageFut<'static>, Pool, Exclusive>;

static WRITEPAGES: Mempool<Kmalloc> = ::pin_init::zeroed();
static WRITEPAGE_BVECS: BvecMempool = ::pin_init::zeroed();

pub fn pools_init(m: &ThisModule) -> Result<(), BchError> {
    Mempool::init_static(&WRITEPAGES, Mempool::kmalloc(4, exclusive_size::<Io, WritepageFut<'static>, Pool>()), m)?;
    BvecMempool::init_static(&WRITEPAGE_BVECS, 4, m)
}

pub fn pools_exit(q: &Quiesced<'_>) {
    WRITEPAGE_BVECS.exit(q);
    WRITEPAGES.exit(q);
}

struct WritepageState {
    io:   Option<IoSlot>,
    opts: c::bch_inode_opts,
}

/// Its io, not started: ours to fill.
fn io_of(slot: &mut IoSlot) -> &mut Io {
    unsafe { slot.home().get_unchecked_mut() }
}

impl c::bch_writepage_io {
    /// Its writeback state - its folios' - is the io's ref on it.
    fn inode(&self) -> &'static InodeInfo {
        unsafe { InodeInfo::from_inode(self.inode.cast()) }
    }
}

impl BioSubmit for c::bch_writepage_io {
    fn bio(&self) -> &Bio {
        &self.wbio.bio
    }

    fn bio_mut(&mut self) -> &mut Bio {
        self.wbio.bio_mut()
    }
}

unsafe impl BioOwner for c::bch_writepage_io {
    type Owns = Data;
}

fn bio_full(bio: &Bio, len: u32) -> bool {
    bio.nr_vecs() >= bio.max_vecs() || bio.range().len() > u32::MAX - len
}

/*
 * Determine when a writepage io is full. We have to limit writepage bios to a
 * single page per bvec (i.e. 1MB with 4k pages) because that is the limit to
 * what the bounce path in bch2_write_extent() can handle. In theory we could
 * loosen this restriction for non-bounce I/O, but we don't have that context
 * here. Ideally, we can up this limit and make it configurable in the future
 * when the bounce path can be enhanced to accommodate larger source bios.
 */
fn io_full(io: &Io, len: u32) -> bool {
    let bio = &io.wbio.bio;
    bio_full(bio, len) || bio.range().len() + len > c::BIO_MAX_VECS * PAGE_SIZE as u32
}

/// Every sector of each of the io's folios left with no replicas - after
/// @f, on the folio.
fn folios_clear_replicas(io: &Io, f: impl Fn(&Folio)) {
    for fv in io.range_all().folios() {
        let folio = unsafe { Folio::from_raw(fv.folio) };
        f(folio);

        let mut s = folio.lock_state();
        for x in s.sectors() {
            x.bits.set_nr_replicas(0);
        }
    }
}

fn writepage_io_done(io: &mut Io) {
    let inode = io.inode();

    if io.op.error != 0 {
        inode.ei_flags.set(c::InodeInfoFlag::EI_INODE_ERROR, true);

        folios_clear_replicas(io, |folio| {
            folio.mapping().expect("a folio in writeback with no mapping").set_error(-(c::EIO as c_int))
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

    for f in io.range_all().folios() {
        drop(unsafe { Writeback::from_io(f.folio) });
    }
}

/// The task's future apart: before rustc 1.87, every item of the module its
/// type is named in that has the type in its signature must be its defining
/// use - here, only the fn that makes it.
mod task {
    use super::*;

    #[in_place(WritepageFut)]
    pub(super) async fn writepage_task<'h>(io: Pin<&'h mut Io>) {
        let io = unsafe { io.get_unchecked_mut() };
        let inode = io.inode();
        write(&mut io.op, &mut io.wbio, WriteFor::Inode(&inode.ei_devs_need_flush), None, Resume::queue(Queue::system_dfl()))
            .await;
        writepage_io_done(io);
    }
}
use task::{writepage_task, WritepageFut};

fn writepage_do_io(w: &mut WritepageState) {
    let mut slot = w.io.take().expect("writepage_do_io() with no io");
    let fs = io_of(&mut slot).op.fs();

    unsafe { slot.start(Spawn::Inline, fs.tasks(), writepage_task) };
}

/*
 * Get a bch_writepage_io and add @page to it - appending to an existing one if
 * possible, else allocating a new one:
 */
fn writepage_io_alloc(fs: &Fs, wbc: &Wbc, w: &mut WritepageState, inode: &InodeInfo,
                      sector: u64, nr_replicas: u32, res_nr_replicas: u32) {
    let opts = w.opts;
    let mut slot = IoSlot::exclusive_bio_in(&WRITEPAGES, &WRITEPAGE_BVECS, c::BIO_MAX_VECS as u16,
                                            c::req_op_REQ_OP_WRITE, |b| {
        pin_init!(Io {
            inode: inode.raw(),
            op    <- c::bch_write_op::init(fs, opts),
            wbio  <- wbio_in_place(b),
        }? BchError)
    }).expect("a writepage io's box: its pool waits for an element, and an op's init can't fail");

    let io = io_of(&mut slot);
    let op = &mut io.op;
    op.target          = opts.foreground_target;
    op.bits.set_nr_replicas(nr_replicas as u8);
    op.res.nr_replicas.set(res_nr_replicas);
    op.write_point     = c::write_point_specifier::hashed(inode.ei_last_dirtied.get() as usize);
    op.subvol          = inode.inum().subvol as u32;
    op.pos             = pos(inode.inum().inum, sector);
    io.wbio.bio_mut().set_sector(sector);
    io.wbio.bio_mut().set_opf(wbc.write_flags());

    w.io = Some(slot);
}

fn throttle_writes(fs: &Fs) {
    let reserved = c::OPEN_BUCKETS_COUNT -
        (c::OPEN_BUCKETS_COUNT - crate::alloc::foreground::open_buckets_reserved(c::Watermark::normal)) / 2;

    let mut start = 0;
    loop {
        let open_buckets_seen = fs.allocator.open_buckets_events.snapshot();
        let journal_seen = fs.journal.async_wait.snapshot();
        let open_buckets_low = unlikely(u32::from(fs.allocator.freelist_lock.open_buckets_nr_free.get()) <= reserved);
        let journal_low = (c::Watermark::normal as u32) < fs.journal.watermark() as u32 && fs.journal.error().is_ok();
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
        fs.times.get(c::TimeStat::blocked_writeback_throttle as usize).update(start);
    }
}

fn writepage(folio: FolioGuard<'_>, wbc: &Wbc, w: &mut WritepageState, tmp: &mut [c::bch_folio_sector]) -> c_int {
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
            let bio = io.wbio.bio.raw();
            if u32::from(io.op.bits.nr_replicas()) != nr_replicas_this_write ||
               io.op.res.nr_replicas.get() != res_nr_replicas ||
               io_full(io, (sectors << 9) as u32) ||
               bio.bi_iter.bi_sector + u64::from(bio.bi_iter.bi_size >> 9) != sector {
                writepage_do_io(w);
            }
        }

        if w.io.is_none() {
            writepage_io_alloc(fs, wbc, w, inode, sector, nr_replicas_this_write, res_nr_replicas);
        }

        folio.share();

        let io = io_of(w.io.as_mut().expect("an io, just made"));
        assert!(core::ptr::eq(io.inode, inode.raw()), "a writepage io of another inode");
        unsafe { io.wbio.bio_mut().add_folio_part(&folio, offset << 9, sectors << 9) };

        io.op.res.sectors.set(io.op.res.sectors.get() + reserved_sectors);
        io.op.i_sectors_delta -= i64::from(dirty_sectors);
        io.op.new_i_size = i_size;

        offset += sectors;
    }

    drop(folio);
    0
}

pub(super) fn writepages(mapping: Mapping, wbc: Wbc) -> c_int {
    let inode = mapping.inode();
    let fs = inode.fs();
    let pool = &fs.vfs.writepage_buf_pool;
    let tmp = {
        let _noio = MemallocFlags::noio();
        Box::<[c::bch_folio_sector], _>::try_new_zeroed_slice_in(
            pool.element_size() / size_of::<c::bch_folio_sector>(), &**pool)
    }.expect("a pool's allocation: it waits for an element");
    // zeroed is a bch_folio_sector
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

    if ret < 0 { BchError::from_raw(ret).class() } else { ret }
}
