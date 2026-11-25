// SPDX-License-Identifier: GPL-2.0

//! Stripe block IO - ec/io.c's - and what reads stripes: the read path's
//! reconstruct, and scrub.

use crate::init::passes::RecoveryPass;
use crate::btree::bkey::{pos, BkeySC, BkeyV};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags};
use crate::c;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::util::alloc::KBox;
use crate::data::ec::buf::{block_checksum, blocks_in, errs_only_dev_offline, errs_to_text, validate, Blocks, MemWait,
                           StripeBuf, EC_BLOCKS_ALL, POST, PRE};
use crate::data::ec::stripe::{ptr_matches_stripe, shape, MAX_BLOCKS};
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, id};
use crate::init::passes;
use crate::data::dev_bio::DevBio;
use crate::sb::members::{BlockDev, DevIoRef, IoRefIdx};
use crate::os::bio::{buf_bio_bytes, buf_nr_bvecs, Bio, Buffer, Gathered, HoldsDev, MapBuf};
use crate::os::blkdev::BlockDevice;
use crate::util::gather::{Fold, Gather, Scope};
use crate::os::kernel::WRITE;
use crate::util::rcu::read_lock;
use crate::util::sleep::Sleep;
use crate::util::task::{waiter, Resume, WaitSync, Waiter};
use crate::os::workqueue::Queue;
use crate::util::vstructs::Validated;
use crate::util::Printbuf;
use crate::{log_msg_ratelimited, prt};
use core::pin::Pin;
use core::sync::atomic::{AtomicI32, Ordering};
use cstruct_macros::args_stored_once;
use pin_init::{pin_data, pin_init, stack_pin_init, PinInit};
use crate::format::{Bpos, Btree, DataType};

// ── Block IO ─────────────────────────────────────────────────────────────
//
// A round of block IO - the blocks a read, or a write, does together - is a
// BlockIo: it borrows the buffer's blocks apart, for 'a, and lends each bio its
// piece of one; a Scope<'a> its bios complete into is waited on - in a task,
// or synchronously - before the buffer's had back. A read borrows the buffer
// &mut, its bios each their piece &mut; a write borrows it &, as its bios do,
// so everything a write round writes is in the buffer before it's begun.
// Each bio holds an io ref on its device, leaked into it as it's submitted
// and adopted back by its endio, which puts it before its Gather ref. A
// Gather is used once, so each round has its own, where C reused the
// buffer's closure. A bio's endio doesn't touch the buffer: its block's error
// goes into the Gather, and the wait hands them back - recorded to err[PRE],
// each block's last, as C's endio left it. The key isn't the round's: each
// submit is handed it, for the block's pointer.


/// Each block's error from a round's bios, if it had one.
#[derive(Clone, Copy, Default)]
pub struct BlockErrs([Option<BchError>; MAX_BLOCKS]);

impl Fold for BlockErrs {
    type Item = Self;
    /// Each block's errcode, raw: 0 for none
    type Acc = [AtomicI32; MAX_BLOCKS];

    fn acc() -> impl PinInit<Self::Acc> {
        core::array::from_fn::<_, MAX_BLOCKS, _>(|_| AtomicI32::new(0))
    }

    fn fold(acc: &Self::Acc, v: Self) {
        for (a, e) in acc.iter().zip(v.0) {
            if let Some(e) = e {
                a.store(e.raw(), Ordering::Relaxed);
            }
        }
    }

    fn result(acc: &Self::Acc) -> Self {
        BlockErrs(core::array::from_fn(|i| match acc[i].load(Ordering::Relaxed) {
            0 => None,
            e => Some(BchError::from_raw(e)),
        }))
    }
}

impl BlockErrs {
    /// Each block's error, or @other's.
    fn or(self, other: Self) -> Self {
        BlockErrs(core::array::from_fn(|i| self.0[i].or(other.0[i])))
    }

    /// Into @buf's err[PRE]: each block's, its last.
    pub(crate) fn record(self, buf: &mut StripeBuf) {
        for (i, e) in self.0.into_iter().enumerate() {
            if e.is_some() {
                buf.err[PRE][i] = e;
            }
        }
    }
}

/// What a round's bios map of its blocks: &mut [u8] to be read into, & to be
/// written from - so which way a round goes is which it borrows.
pub(crate) trait RoundBuf: MapBuf + Sized + Send {
    type Rw: IoRefIdx;

    fn split_at(self, mid: usize) -> (Self, Self);
}

impl RoundBuf for &mut [u8] {
    type Rw = c::DevReadRefKind;

    fn split_at(self, mid: usize) -> (Self, Self) {
        self.split_at_mut(mid)
    }
}

impl RoundBuf for &[u8] {
    type Rw = c::DevWriteRefKind;

    fn split_at(self, mid: usize) -> (Self, Self) {
        <[u8]>::split_at(self, mid)
    }
}

/// One bio of a round, from ec.block_bioset: its piece of a block.
#[pin_data]
#[repr(C)]
pub struct EcBio<M: MapBuf> {
    /// The block's pointer, for the endio's staleness check
    ptr: c::bch_extent_ptr,
    idx: u8,
    #[pin]
    dev: DevBio<Buffer<M>>,
}

crate::has_bio!(EcBio<M>, dev.bio, owns Buffer<M> where M: MapBuf);

impl<M: MapBuf> HoldsDev for EcBio<M> {
    type Dev = BlockDev;

    fn hold(self: Pin<&mut Self>, ca: BlockDev) {
        self.project().dev.hold(ca)
    }

    fn bdev(&self) -> &BlockDevice {
        self.dev.bdev()
    }
}

/// A round's bio done: its IO accounted against the device, and its block's
/// error folded into the round's Gather - the device's, or its pointer gone
/// stale.
impl<M: MapBuf> Gathered<BlockErrs> for EcBio<M> {
    fn done(self: Pin<&mut Self>, status: Result<(), BchError>) -> BlockErrs {
        let this = self.project();
        let (ca, _) = this.dev.end();
        let e = match status {
            Err(e) => Some(e),
            Ok(()) if ca.dev_ptr_stale(this.ptr, &read_lock()) => Some(ca.fs().err(Errcode::stripe_read_ptr_stale)),
            Ok(()) => None,
        };
        let mut errs = BlockErrs::default();
        errs.0[usize::from(*this.idx)] = e;
        errs
    }
}

/// A round of block IO on a buffer's blocks, M: read into them, or written
/// from them, as RoundBuf says; waited on by @Wt, its bios gathered in @done.
pub(crate) struct BlockIo<'a, M: RoundBuf, Wt: Waiter> {
    /// Waited on before the buffer's had back: its drop asserts it was
    done:   Scope<'a, BlockErrs, Wt>,
    fs:     &'a Fs,
    /// Each block, till a submit lends it out
    blocks: Blocks<M>,
    /// The blocks whose device was offline, or pointer stale, at submit
    errs:   BlockErrs,
}

impl<'a, Wt: Waiter> BlockIo<'a, &'a mut [u8], Wt> {
    pub(crate) fn read(fs: &'a Fs, buf: &'a mut StripeBuf, done: &'a Gather<BlockErrs, Wt>) -> Self {
        Self::new(fs, buf.blocks_apart_mut(), done)
    }
}

impl<'a, Wt: Waiter> BlockIo<'a, &'a [u8], Wt> {
    pub(crate) fn write(fs: &'a Fs, buf: &'a StripeBuf, done: &'a Gather<BlockErrs, Wt>) -> Self {
        Self::new(fs, buf.blocks_apart(), done)
    }
}

impl<'a, M: RoundBuf + 'a, Wt: Waiter> BlockIo<'a, M, Wt>
where
    DevIoRef<M::Rw>: Into<BlockDev>,
{
    fn new(fs: &'a Fs, blocks: Blocks<M>, done: &'a Gather<BlockErrs, Wt>) -> Self {
        Self { done: Scope::new(done), fs, blocks, errs: BlockErrs::default() }
    }

    /// Block @idx of stripe @s, what the buffer has of it.
    pub(crate) fn submit(&mut self, s: BkeySC<'_, c::Stripe<Validated>>, idx: usize) {
        let (offset, size) = (self.blocks.offset, self.blocks.size);
        self.submit_range(s, idx, offset, size)
    }

    /// Block @idx of stripe @s, @sectors from @sector_offset - into the
    /// stripe, within what the buffer has - read into its buffer, or written
    /// from it: bch2_ec_block_io_range(). A block's IO is once a round. Its
    /// device offline, or its pointer stale, is the block's error at once.
    pub(crate) fn submit_range(&mut self, s: BkeySC<'_, c::Stripe<Validated>>, idx: usize,
                               sector_offset: u32, sectors: u32) {
        let this = self;
        let fs = this.fs;
        let sh = shape(s);
        let (offset, size) = (this.blocks.offset, this.blocks.size);
        assert_eq!(sh.nr_blocks, this.blocks.nr_blocks, "a stripe buffer, handed a stripe of another width");
        let block = this.blocks.blocks[idx].take()
            .unwrap_or_else(|| panic!("IO to block {idx} of a stripe buffer without it, or twice in a round"));
        assert!(sector_offset >= offset && sector_offset + sectors <= offset + size,
                "IO to {sector_offset}+{sectors} of a stripe buffer of {offset}+{size}");

        let ptr = s.v().block_ptrs()[idx];
        let data_type = if (idx as u32) < sh.nr_data { DataType::user }
                        else { DataType::parity };

        let Some(ca) = fs.dev_get_ioref::<M::Rw>(ptr.dev().into()) else {
            this.errs.0[idx] = Some(fs.err(Errcode::stripe_read_device_offline));
            return;
        };
        if ca.dev_ptr_stale(&ptr, &read_lock()) {
            this.errs.0[idx] = Some(fs.err(Errcode::stripe_read_ptr_stale));
            return;
        }
        ca.io_done_add(M::Rw::RW, data_type, sectors.into());

        let opf = if M::Rw::RW == WRITE { c::req_op_REQ_OP_WRITE } else { c::req_op_REQ_OP_READ };
        let bytes = (sectors as usize) << 9;
        let start = ((sector_offset - offset) as usize) << 9;
        let (mut rest, _) = block.split_at(start).1.split_at(bytes);
        let set = &fs.ec.block_bioset;

        let mut done = 0;
        while done < bytes {
            // @start is a block's, not a page's: where each bio starts in its
            // page says how much it maps
            let (p, len) = rest.as_raw();
            let b = buf_bio_bytes(p, len);
            let nr_vecs = buf_nr_bvecs(p, b);
            let (chunk, tail) = rest.split_at(b);
            rest = tail;
            let bio = set.alloc_as::<EcBio<M>, BchError, _>(nr_vecs as u16, opf, |at| pin_init!(EcBio {
                ptr,
                idx: idx as u8,
                dev <- DevBio::new(at.init_from(|bio| Ok::<_, BchError>(Buffer::fill(bio, chunk)))),
            }? BchError)).expect("a bio from the stripe bioset's mempool");

            bio.submit_gathered(ca.clone().into(), ptr.offset() + u64::from(sector_offset) + (done >> 9) as u64,
                                this.done.get());
            done += b;
        }
    }
}

/// Every bio done: each block's error, to record() into the buffer.
impl<M: RoundBuf> WaitSync for &mut BlockIo<'_, M, waiter::Sync> {
    type Output = BlockErrs;

    fn wait_sync(self, how: Sleep) -> BlockErrs {
        self.done.wait_sync(how).or(self.errs)
    }
}

impl<M: RoundBuf> BlockIo<'_, M, waiter::Async> {
    /// As wait_sync(), awaited: resumed @at.
    pub(crate) async fn wait(&mut self, at: Resume<'_>) -> BlockErrs {
        self.done.wait(at).await.or(self.errs)
    }
}

/// The blocks of @s in @blocks read into @buf, waited on.
pub(crate) fn read_sync(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, buf: &mut StripeBuf, blocks: u32) {
    let errs = {
        stack_pin_init!(let done = Gather::new());
        let mut io = BlockIo::<_, waiter::Sync>::read(fs, buf, &done);
        for i in blocks_in(blocks & ((1u64 << s.v().nr_blocks) - 1) as u32) {
            io.submit(s, i);
        }
        io.wait_sync(Sleep::IDLE)
    };
    errs.record(buf);
}

// ── Scrub ────────────────────────────────────────────────────────────────

/// What scrub found of a block: the sectors in granules that don't match its
/// checksums, and the first one's checksum, as it should be and as it is.
pub struct Scrubbed {
    pub bad_sectors: u64,
    pub want:        c::bch_csum,
    pub got:         c::bch_csum,
}

/// Scrub: block @block of stripe @s checked against the stripe's checksums
/// for it, read a checksum granule at a time - parity, and data no extent
/// references any more, are still inputs to reconstruct, and nothing else
/// ever reads them. The error if the block couldn't
/// be read, including its pointer going stale: the stripe deleted, or
/// reused, under us. Its buffer, of one granule, is its own.
pub fn scrub_block(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, block: usize) -> Result<Scrubbed, BchError> {
    let sh = shape(s);
    let mut ret = Scrubbed { bad_sectors: 0, want: Default::default(), got: Default::default() };
    if sh.csum_type == c::CsumType::none {
        return Ok(ret);
    }
    let granularity = 1u32 << sh.csum_granularity_bits;

    let mut buf = StripeBuf::new(fs, s, 1 << block, 0, granularity.min(sh.sectors), MemWait::Never)?;
    for offset in (0..sh.sectors).step_by(granularity as usize) {
        buf.slide(s, offset);
        read_sync(fs, s, &mut buf, 1 << block);

        if let Some(e) = buf.err[PRE][block] {
            return Err(e);
        }

        let want = s.v().csums().ok_or_else(|| fs.err(Errcode::stripe_unknown_csum_type))?
            .get(block as u32, offset >> sh.csum_granularity_bits);
        let got = block_checksum(s, &buf, block as u32, offset);
        if want != got {
            if ret.bad_sectors == 0 {
                (ret.want, ret.got) = (want, got);
            }
            ret.bad_sectors += u64::from(buf.size);
        }
    }

    if ret.bad_sectors != 0 {
        if let Some(ca) = fs.dev_get(s.v().block_ptrs()[block].dev().into()) {
            ca.io_error(c::MemberErrorType::checksum);
        }
    }
    Ok(ret)
}

// ── The read path's reconstruct ──────────────────────────────────────────

/// @pick's extent - at @data_pos in @btree - read into @bio from its stripe instead:
/// reconstructed from the rest of the stripe, its block having failed. What
/// went wrong into @msg, for the read's error message.
///
/// The stripe's pointers are checked for staleness under the btree locks the
/// stripe key was read with, the extent seen still pointing into it in the
/// same transaction: a live stripe pointing into a reused bucket is an
/// allocator inconsistency, open or not. The extent changed is the race a
/// stale pointer is. After the IO, validate() checks again, and there the
/// stripe being open is what makes a stale pointer one - the key may have
/// been deleted while the IO was in flight.
#[args_stored_once]
pub(crate) async fn read_extent(fs: &Fs, btree: Btree, data_pos: Bpos, bio: &mut Bio,
                                pick: &c::extent_ptr_decoded, flags: c::ReadFlags, msg: &mut Printbuf)
    -> Result<(), BchError>
{
    let iter = c::bvec_iter { bi_sector: pick.ptr.offset(), ..bio.raw().bi_iter };
    let (key, mut buf, block) = reconstruct_start(fs, btree, data_pos, iter, pick, flags, msg)?;
    let errs = {
        stack_pin_init!(let done = Gather::new());
        let mut io = BlockIo::<_, waiter::Async>::read(fs, &mut buf, &done);
        {
            let s = BkeySC::from(&key).arm::<c::Stripe>().expect("a stripe, as looked up");
            for i in blocks_in(EC_BLOCKS_ALL & ((1u64 << s.v().nr_blocks) - 1) as u32) {
                io.submit(s, i);
            }
        }
        io.wait(Resume::queue(Queue::system_dfl())).await
    };
    errs.record(&mut buf);
    reconstruct_finish(fs, &key, &mut buf, block, bio, iter, flags, msg)
}

/// The stripe looked up and checked, and its buffer: everything before the IO.
fn reconstruct_start(fs: &Fs, btree: Btree, data_pos: Bpos, iter: c::bvec_iter,
                     pick: &c::extent_ptr_decoded, flags: c::ReadFlags, msg: &mut Printbuf)
    -> Result<(BkeyBuf, KBox<StripeBuf>, usize), BchError>
{
    assert!(pick.has_ec, "reconstructing a read of an extent with no stripe");
    let mut key = BkeyBuf::new();
    let mut stale = c::StripeBlockMask::default();
    let found = {
        let trans = crate::btree_trans!(fs);
        lockrestart_do(&trans, |t| {
            let mut iter = BtreeIter::new(t, btree, data_pos, BtreeIterFlags::empty());
            // @pick may have been moved to the part of the extent read, with
            // @data_pos: matched by where its data starts, as narrow_crcs does
            let data_offset = data_pos.offset - u64::from(pick.crc.offset);
            let extent = iter.peek_slot(t)?;
            if !extent.is_some_and(|k| k.arm::<Ptrs>().is_some_and(|kp| kp.matches_ptr(fs, &pick.ptr, data_offset))) {
                return Ok(None);
            }

            let mut iter = BtreeIter::new(t, Btree::stripes, pos(0, pick.ec.idx()), BtreeIterFlags::SLOTS);
            let Some(s) = iter.peek_slot(t)?.expect("a slot always has a key").arm::<c::Stripe>() else {
                return Ok(Some(false));
            };
            key.set_key(s.untyped());
            stale = c::StripeBlockMask::default();
            let rcu = read_lock();
            for (i, p) in s.v().block_ptrs().iter().enumerate() {
                if fs.dev_rcu(p.dev().into(), &rcu).is_some_and(|ca| ca.dev_ptr_stale(p, &rcu)) {
                    stale.set(i);
                }
            }
            Ok(Some(true))
        })
    };
    match found {
        Ok(Some(true)) => {}
        Ok(None) => return Err(crate::data::read::stale(fs, flags)),
        _ => {
            prt!(msg, "error looking up stripe\n");
            return fs.throw(Errcode::stripe_reconstruct);
        }
    }

    let s = BkeySC::from(&key).arm::<c::Stripe>().expect("a stripe, as looked up");
    if !ptr_matches_stripe(s, pick) {
        prt!(msg, "pointer doesn't match stripe\n{}\n", s.to_text(fs));
        return fs.throw(Errcode::stripe_reconstruct);
    }

    let block = usize::from(pick.ec.block());
    let sectors = iter.bi_size >> 9;
    let offset = (iter.bi_sector - s.v().block_ptrs()[block].offset()) as u32;
    if offset + sectors > u32::from(s.v().sectors.get()) {
        prt!(msg, "read is bigger than stripe\n{}\n", s.to_text(fs));
        return fs.throw(Errcode::stripe_reconstruct);
    }

    if stale != c::StripeBlockMask::default() {
        let mut m = log_msg_ratelimited!(fs);
        prt!(&mut m, "Stripe with stale pointer(s):\n{}", s.to_text(fs));
        count_fsck_err(fs, id::stale_dirty_ptr, &mut m);
        let _ = passes::run_explicit(fs, &mut m, RecoveryPass::check_allocations,
                                     c::RunRecoveryPassFlags::from_bits_retain(0));
    }

    let Ok(mut buf) = StripeBuf::new(fs, s, EC_BLOCKS_ALL, offset, sectors, MemWait::Never) else {
        prt!(msg, "error allocating stripe data buffers\n{}\n", s.to_text(fs));
        return fs.throw(Errcode::stripe_reconstruct_enomem);
    };
    buf.stale = stale;
    Ok((key, buf, block))
}

/// The IO done: what it read checked, and @block's copied into @bio - @iter
/// its position, at the pick's sector.
fn reconstruct_finish(fs: &Fs, key: &BkeyBuf, buf: &mut StripeBuf, block: usize, bio: &mut Bio,
                      iter: c::bvec_iter, flags: c::ReadFlags, msg: &mut Printbuf) -> Result<(), BchError> {
    let s = BkeySC::from(key).arm::<c::Stripe>().expect("a stripe, as looked up");
    let (sectors, offset) = (iter.bi_size >> 9, (iter.bi_sector - s.v().block_ptrs()[block].offset()) as u32);

    // A block went stale under us: the stripe was deleted or replaced, so
    // the extent has changed. Retry, as a direct read of a stale pointer is.
    let ret = validate(fs, s, buf, false, EC_BLOCKS_ALL);
    if ret.is_err_and(|e| e == BchError::from(Errcode::stripe_reconstruct_stale_race)) {
        return Err(crate::data::read::stale(fs, flags));
    }

    if ret.is_ok() {
        let from = ((offset - buf.offset) as usize) << 9;
        let src = &buf.block(block)[from..from + ((sectors as usize) << 9)];
        bio.range().copy_from(src);
    }

    if ret.is_ok() && errs_only_dev_offline(buf) {
        return Ok(());
    }

    errs_to_text(msg, fs, s, buf, false, EC_BLOCKS_ALL);

    if buf.nr_failed(PRE) != 0 || buf.nr_failed(POST) != 0 {
        match ret {
            Err(e) => prt!(msg, "error: {e}\n"),
            Ok(()) => prt!(msg, "successful reconstruct\n"),
        }
    }
    ret
}
