// SPDX-License-Identifier: GPL-2.0

//! Buffered writeback: buffered.c's writepages. A writeback walk's dirty
//! sectors, gathered into bch_writepage_ios - one bio's worth of contiguous
//! sectors at one replica count each - and written by bch2_write(), each
//! io's end_io ending its folios' writeback.

use crate::btree::bkey::pos;
use crate::c;
use crate::data::write::bch2_write;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::inode_opts;
use crate::locking::MemallocFlags;
use crate::util::alloc::AllocBox;
use crate::util::bio::{plugged, Bio, BioBuffersUnmanaged, BioOwner};
use crate::util::closure::{self, closure_wait, StackClosure};
use crate::util::kernel::{local_clock, unlikely, PAGE_SIZE};
use crate::util::ops::from_raw;
use crate::util::time_stats::TimeStats;
use crate::vfs::folio::{Folio, FolioGuard, FolioSpan, Mapping};
use crate::vfs::inode::InodeInfo;
use c::bch_folio_sector_state as SectorState;
use c::bch_write_flags as w;
use core::ffi::c_int;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::{AcqRel, Relaxed};

type Io = c::bch_writepage_io;

struct WritepageState {
    io:   Option<NonNull<Io>>,
    opts: c::bch_inode_opts,
    tmp:  *mut c::bch_folio_sector,
}

/// The VFS's writeback of a mapping, for the writepages() it's passed to.
pub(crate) struct Wbc(NonNull<c::writeback_control>);

from_raw! {
    Wbc = *mut c::writeback_control => |r| Wbc(NonNull::new_unchecked(r));
}

impl Wbc {
    /// The next folio to write back, locked - after @prev, which writing
    /// returned @err: writeback_iter().
    fn next(&self, mapping: Mapping, prev: Option<Folio>, err: &mut c_int) -> Option<FolioGuard<'_>> {
        let prev = prev.map_or(core::ptr::null_mut(), Folio::as_ptr);
        let folio = unsafe { c::writeback_iter(mapping.as_ptr(), self.0.as_ptr(), prev, err) };
        (!folio.is_null()).then(|| unsafe { FolioGuard::adopt(Folio::from_raw(folio)) })
    }

    /// wbc_to_write_flags(): what its writes are submitted as.
    fn write_flags(&self) -> c::blk_opf_t {
        unsafe { c::wbc_to_write_flags(self.0.as_ptr()) }
    }
}

impl c::bch_writepage_io {
    /// Its writeback state - its folios' - is the io's ref on it.
    fn inode(&self) -> &'static InodeInfo {
        unsafe { InodeInfo::from_inode(self.inode.cast()) }
    }
}

unsafe impl BioOwner for c::bch_writepage_io {
    type Buffers = BioBuffersUnmanaged;

    fn bio(&self) -> &Bio {
        unsafe { Bio::from_raw(&self.op.wbio.bio) }
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        unsafe { Bio::from_raw_mut(&mut self.op.wbio.bio) }
    }
}

/// The io of a write op's end_io.
///
/// # Safety
/// @op is a bch_writepage_io's.
unsafe fn of_op(op: *mut c::bch_write_op) -> *mut Io {
    op.wrapping_byte_sub(core::mem::offset_of!(Io, op)).cast()
}

fn bio_full(bio: &c::bio, len: u32) -> bool {
    bio.bi_vcnt >= bio.bi_max_vecs || bio.bi_iter.bi_size > u32::MAX - len
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
    let bio = &io.op.wbio.bio;
    bio_full(bio, len) || bio.bi_iter.bi_size + len > c::BIO_MAX_VECS * PAGE_SIZE as u32
}

/// Every sector of each of the io's folios left with no replicas - after
/// @f, on the folio.
fn folios_clear_replicas(io: &Io, f: impl Fn(Folio)) {
    for fv in io.range_all().folios() {
        let folio = unsafe { Folio::from_raw(fv.folio.as_ptr()) };
        f(folio);

        let mut s = folio.lock_state();
        for x in s.sectors() {
            x.bits.set_nr_replicas(0);
        }
    }
}

unsafe fn writepage_io_done(op: *mut c::bch_write_op) {
    let io = unsafe { &mut *of_op(op) };
    let inode = io.inode();

    if io.op.error != 0 {
        inode.ei_flags.set(c::bch_inode_info_flags::EI_INODE_ERROR, true);

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
        let folio = unsafe { Folio::from_raw(f.folio.as_ptr()) };
        let s = unsafe { folio.state_ptr().expect("a folio in writeback with no bch_folio").as_ref() };

        if s.write_count.fetch_sub(1, AcqRel) == 1 {
            unsafe { folio.end_writeback() };
        }
    }

    unsafe { c::bio_put(&mut io.op.wbio.bio) };
}

fn writepage_do_io(w: &mut WritepageState) {
    let io = w.io.take().expect("writepage_do_io() with no io");

    unsafe { closure::call(NonNull::from(&mut (*io.as_ptr()).op), bch2_write, None) };
}

/*
 * Get a bch_writepage_io and add @page to it - appending to an existing one if
 * possible, else allocating a new one:
 */
fn writepage_io_alloc(fs: &Fs, wbc: &Wbc, w: &mut WritepageState, inode: &InodeInfo,
                      sector: u64, nr_replicas: u32, res_nr_replicas: u32) {
    let bio = unsafe {
        c::bio_alloc_bioset(core::ptr::null_mut(), c::BIO_MAX_VECS as u16, c::req_op_REQ_OP_WRITE,
                            c::GFP_KERNEL, fs.vfs.writepage_bioset.as_ptr())
    };
    let io = unsafe { crate::util::bio::of_bio::<Io>(bio) };
    let i = unsafe { &mut *io };

    i.inode = inode.raw();
    unsafe { c::bch_write_op::init_at(&raw mut i.op, fs, w.opts) };
    let op = &mut i.op;
    op.target          = w.opts.foreground_target;
    op.bits.set_nr_replicas(nr_replicas as u8);
    op.res.nr_replicas.set(res_nr_replicas);
    op.write_point     = c::write_point_specifier::hashed(inode.ei_last_dirtied.get() as usize);
    op.subvol          = inode.inum().subvol as u32;
    op.pos             = pos(inode.inum().inum, sector);
    op.end_io          = Some(writepage_io_done);
    op.devs_need_flush = &inode.ei_devs_need_flush;
    op.wbio.bio.bi_iter.bi_sector = sector;
    op.wbio.bio.bi_opf = wbc.write_flags();

    w.io = NonNull::new(io);
}

fn can_write_now(fs: &Fs, cl: &StackClosure) -> bool {
    let reserved = c::OPEN_BUCKETS_COUNT -
        (c::OPEN_BUCKETS_COUNT - crate::alloc::foreground::open_buckets_reserved(c::bch_watermark::normal)) / 2;
    let cl = NonNull::new(cl.as_ptr()).expect("a closure on the stack");

    if unlikely(u32::from(fs.allocator.freelist_lock.open_buckets_nr_free.get()) <= reserved) {
        unsafe { closure_wait(&fs.allocator.open_buckets_wait, cl) };
        return false;
    }

    if (c::bch_watermark::normal as u32) < fs.journal.watermark() as u32 && fs.journal.error().is_ok() {
        unsafe { closure_wait(&fs.journal.async_wait, cl) };
        return false;
    }

    true
}

fn throttle_writes(fs: &Fs, cl: &StackClosure) {
    let mut start = 0;
    while !can_write_now(fs, cl) {
        if start == 0 {
            start = local_clock();
        }
        cl.sync();
    }

    assert!(cl.nr_remaining() <= 1, "writeback throttle's closure still waiting");

    if start != 0 {
        fs.times[c::bch_time_stats::blocked_writeback_throttle as usize].update(start);
    }
}

fn writepage(folio: FolioGuard<'_>, wbc: &Wbc, w: &mut WritepageState) -> c_int {
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
            inode.undirty_folio(FolioSpan::whole(*folio));
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

        inode.undirty_folio(FolioSpan::new(*folio, i_size, folio.end_pos()));
    }

    let f_sectors = folio.nr_sectors();
    let s = unsafe { folio.state_ptr().expect("a dirty folio with no bch_folio").as_ref() };

    /*
     * Things get really hairy with errors during writeback:
     */
    folio.reserve_writeback(inode).expect("writeback's disk reservation can't fail");

    /* Before unlocking the page, get copy of reservations: */
    let tmp = unsafe { core::slice::from_raw_parts_mut(w.tmp, f_sectors) };
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

    assert!(s.write_count.load(Relaxed) == 0, "a folio with writes in flight, written back again");
    s.write_count.store(1, Relaxed);

    assert!(!folio.writeback(), "a folio already under writeback, written back again");
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
        if let Some(io) = w.io {
            let io = unsafe { io.as_ref() };
            let bio = &io.op.wbio.bio;
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

        s.write_count.fetch_add(1, Relaxed);

        let io = unsafe { w.io.expect("an io, just made").as_mut() };
        assert!(core::ptr::eq(io.inode, inode.raw()), "a writepage io of another inode");
        unsafe { c::bio_add_folio_nofail(&mut io.op.wbio.bio, folio.as_ptr(), sectors << 9, offset << 9) };

        io.op.res.sectors.set(io.op.res.sectors.get() + reserved_sectors);
        io.op.i_sectors_delta -= i64::from(dirty_sectors);
        io.op.new_i_size = i_size;

        offset += sectors;
    }

    if s.write_count.fetch_sub(1, AcqRel) == 1 {
        unsafe { folio.end_writeback() };
    }

    0
}

pub(super) fn writepages(mapping: Mapping, wbc: Wbc) -> c_int {
    let inode = mapping.inode();
    let fs = inode.fs();
    let pool = &fs.vfs.writepage_buf_pool;
    let tmp = {
        let _noio = MemallocFlags::noio();
        AllocBox::<[core::mem::MaybeUninit<u8>], _>::new_uninit_slice(&**pool, pool.element_size())
    }.expect("a pool's allocation: it waits for an element");
    let mut w = WritepageState {
        io:   None,
        opts: inode_opts::get_inode(fs, &inode.inode()),
        tmp:  tmp.as_ptr().as_ptr().cast(),
    };

    let ret = plugged(|| {
        pin_init::stack_pin_init!(let cl = StackClosure::new());

        let mut prev = None;
        let mut ret = 0;

        loop {
            throttle_writes(fs, &cl);
            let Some(folio) = wbc.next(mapping, prev, &mut ret) else { break };
            prev = Some(*folio);
            ret = writepage(folio, &wbc, &mut w);
        }

        if w.io.is_some() {
            writepage_do_io(&mut w);
        }
        ret
    });

    drop(tmp);

    if ret < 0 { BchError::from_raw(ret).class() } else { ret }
}
