// SPDX-License-Identifier: GPL-2.0

//! Stripe block IO - ec/io.c's - and what reads stripes: the read path's
//! reconstruct, and scrub.

use crate::btree::bkey::{pos, BkeySC, BkeyV};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans};
use crate::c;
use crate::data::ec::buf::{block_checksum, blocks_in, errs_only_dev_offline, errs_to_text, validate, MemWait, StripeBuf,
                           EC_BLOCKS_ALL, POST, PRE};
use crate::data::ec::stripe::{ptr_matches_stripe, shape, MAX_BLOCKS};
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, id};
use crate::init::passes;
use crate::sb::members::{DevIoRef, IoRefIdx};
use crate::util::bio::{Bio, BioBox, BioOwner, Endio, RawBio};
use crate::util::ffi::Opaque;
use crate::util::gather::{Fold, Gather, GatherRef};
use crate::util::kernel::{local_clock, PAGE_SIZE, WRITE};
use crate::util::rcu::read_lock;
use crate::util::task::{Resumable, Resume};
use crate::util::vstructs::Validated;
use crate::util::Printbuf;
use crate::{log_msg_ratelimited, prt};
use core::ffi::{c_int, c_uint};
use core::marker::PhantomData;
use core::pin::Pin;
use core::sync::atomic::{AtomicI32, Ordering};
use cstruct_macros::c_export;
use pin_init::{pin_data, pin_init, stack_pin_init, PinInit};

// ── Block IO ─────────────────────────────────────────────────────────────
//
// A round of block IO - the blocks a read, or a write, does together - is a
// BlockIo: it has the buffer for as long, and a Gather its bios complete into,
// waited on - in a task, or synchronously - before the buffer's had back. Each
// bio holds an io ref on its device, leaked into it as it's submitted and
// adopted back by its endio, which puts it before its Gather ref. A Gather is
// used once, so each round has its own, where C reused the buffer's closure.
// A bio's endio doesn't touch the buffer, which the round has by &mut: its
// block's error goes into the Gather, and the wait writes them to err[PRE] -
// each block's last, as C's endio left it. The key isn't the round's: each
// submit is handed it, for the block's pointer - so between submits the key
// can be written, a new stripe's checksums generated.


/// Each block's error from a round's bios, if it had one.
#[derive(Clone, Copy, Default)]
pub struct BlockErrs([Option<BchError>; MAX_BLOCKS]);

impl Fold for BlockErrs {
    /// Each block's errcode, raw: 0 for none
    type Acc = [AtomicI32; MAX_BLOCKS];

    fn acc() -> Self::Acc {
        core::array::from_fn(|_| AtomicI32::new(0))
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

/// One bio of a round, from ec.block_bioset.
#[pin_data]
#[repr(C)]
pub struct EcBio {
    /// Its block's device, kept by the bio's io ref on it, for the endio
    ca:          Option<BlockDev>,
    /// A ref on the round's Gather, for the endio to put
    done:        Option<GatherRef<BlockErrs>>,
    /// The block's pointer, for the endio's staleness check
    ptr:         c::bch_extent_ptr,
    submit_time: u64,
    idx:         u8,
    #[pin]
    bio:         Bio,
}

crate::has_bio!(EcBio, bio);

/// A block IO's device, by its io ref: a read's, or a write's.
pub(crate) enum BlockDev {
    Read(DevIoRef<c::bch_dev_read_ref>),
    Write(DevIoRef<c::bch_dev_write_ref>),
}

impl From<DevIoRef<c::bch_dev_read_ref>> for BlockDev {
    fn from(ca: DevIoRef<c::bch_dev_read_ref>) -> Self {
        BlockDev::Read(ca)
    }
}

impl From<DevIoRef<c::bch_dev_write_ref>> for BlockDev {
    fn from(ca: DevIoRef<c::bch_dev_write_ref>) -> Self {
        BlockDev::Write(ca)
    }
}

/// A round's bio done: its IO accounted against the device, and its block's
/// error folded into the round's Gather - the device's, or its pointer gone
/// stale.
struct BlockEndio;

impl Endio<BioBox<EcBio>> for BlockEndio {
    fn endio(mut bio: BioBox<EcBio>) {
        let this = bio.as_mut().project();
        let done = this.done.take().expect("a round's bio, with its ref");
        let ca = this.ca.take().expect("a round's bio, with its device");
        let (ptr, idx, submit_time) = (bio.ptr, bio.idx, bio.submit_time);
        let status = bio.bio.status();
        drop(bio);

        let e = match ca {
            BlockDev::Read(ca)  => block_done(ca, &ptr, submit_time, status),
            BlockDev::Write(ca) => block_done(ca, &ptr, submit_time, status),
        };
        let mut errs = BlockErrs::default();
        errs.0[usize::from(idx)] = e;
        done.put(errs);
    }
}

/// A bio's IO, on block pointer @ptr, accounted against its device - @ca, its
/// io ref, put as this returns: its error, or its pointer gone stale.
fn block_done<R: IoRefIdx>(ca: DevIoRef<R>, ptr: &c::bch_extent_ptr, submit_time: u64,
                           status: Result<(), BchError>) -> Option<BchError> {
    use c::bch_member_error_type as T;
    let write = R::RW == WRITE;
    ca.account_io_completion(if write { T::BCH_MEMBER_ERROR_write } else { T::BCH_MEMBER_ERROR_read },
                             submit_time, status.is_ok());
    if write {
        ca.write_unflushed();
    }

    match status {
        Err(e) => Some(e),
        Ok(()) if ca.ptr_stale(ptr, &read_lock()).is_none_or(|n| n != 0) =>
            Some(ca.fs().err(bch_errcode::BCH_ERR_stripe_read_ptr_stale)),
        Ok(()) => None,
    }
}

/// A round of block IO on @buf: reads, or writes, as @R's io ref is.
#[pin_data]
pub(crate) struct BlockIo<'a, R: IoRefIdx> {
    /// Waited on, or its drop waits, before the buffer's had back
    #[pin]
    done:    Gather<BlockErrs>,
    fs:      &'a Fs,
    buf:     &'a mut StripeBuf,
    _r:      PhantomData<fn() -> R>,
}

impl<'a> BlockIo<'a, c::bch_dev_read_ref> {
    pub(crate) fn read(fs: &'a Fs, buf: &'a mut StripeBuf) -> impl PinInit<Self> + 'a {
        Self::new(fs, buf)
    }
}

impl<'a> BlockIo<'a, c::bch_dev_write_ref> {
    pub(crate) fn write(fs: &'a Fs, buf: &'a mut StripeBuf) -> impl PinInit<Self> + 'a {
        Self::new(fs, buf)
    }
}

impl<'a, R: IoRefIdx> BlockIo<'a, R>
where
    DevIoRef<R>: Into<BlockDev>,
{
    fn new(fs: &'a Fs, buf: &'a mut StripeBuf) -> impl PinInit<Self> + 'a {
        pin_init!(Self { done <- Gather::new(), fs, buf, _r: PhantomData })
    }

    /// The buffer, between submits: to zero what's to be written, say.
    pub(crate) fn buf(self: Pin<&mut Self>) -> &mut StripeBuf {
        self.project().buf
    }

    /// Block @idx of stripe @s, what the buffer has of it.
    pub(crate) fn submit(self: Pin<&mut Self>, s: BkeySC<'_, c::Stripe<Validated>>, idx: usize) {
        let (offset, size) = (self.buf.offset, self.buf.size);
        self.submit_range(s, idx, offset, size)
    }

    /// Block @idx of stripe @s, @sectors from @sector_offset - into the
    /// stripe, within what the buffer has - read into its buffer, or written
    /// from it: bch2_ec_block_io_range(). Its device offline, or its pointer
    /// stale, is the block's error at once.
    pub(crate) fn submit_range(self: Pin<&mut Self>, s: BkeySC<'_, c::Stripe<Validated>>, idx: usize,
                               sector_offset: u32, sectors: u32) {
        let this = self.project();
        let fs = *this.fs;
        let sh = this.buf.shape(s);
        assert!(this.buf.has(idx), "IO to block {idx} of a stripe buffer without it");
        assert!(sector_offset >= this.buf.offset && sector_offset + sectors <= this.buf.offset + this.buf.size,
                "IO to {sector_offset}+{sectors} of a stripe buffer of {}+{}", this.buf.offset, this.buf.size);

        let ptr = s.v().block_ptrs()[idx];
        let data_type = if (idx as u32) < sh.nr_data { c::bch_data_type::BCH_DATA_user }
                        else { c::bch_data_type::BCH_DATA_parity };

        let Some(ca) = fs.dev_get_ioref::<R>(ptr.dev().into()) else {
            this.buf.err[PRE][idx] = Some(fs.err(bch_errcode::BCH_ERR_stripe_read_device_offline));
            return;
        };
        if ca.ptr_stale(&ptr, &read_lock()).is_none_or(|n| n != 0) {
            this.buf.err[PRE][idx] = Some(fs.err(bch_errcode::BCH_ERR_stripe_read_ptr_stale));
            return;
        }
        ca.io_done_add(R::RW, data_type, sectors.into());

        let write = R::RW == WRITE;
        let opf = if write { c::req_op_REQ_OP_WRITE } else { c::req_op_REQ_OP_READ };
        let bytes = (sectors as usize) << 9;
        let nr_vecs = (c::BIO_MAX_VECS as usize).min(bytes.div_ceil(PAGE_SIZE));
        let start = ((sector_offset - this.buf.offset) as usize) << 9;
        let base = this.buf.block_mut(idx)[start..start + bytes].as_mut_ptr();
        let set = &fs.ec.block_bioset;

        let mut offset = 0;
        while offset < bytes {
            let b = (bytes - offset).min(nr_vecs * PAGE_SIZE);
            let done = Some(this.done.get());
            let dev = Some(ca.clone().into());
            let mut bio = set.alloc::<BchError, _>(ca.disk_sb.bdev, nr_vecs as u16, opf, |at| pin_init!(EcBio {
                ca: dev,
                done,
                ptr,
                submit_time: local_clock(),
                idx: idx as u8,
                bio <- at.init::<BchError>(),
            }? BchError)).expect("a bio from the stripe bioset's mempool");

            bio.bio_mut().set_sector(ptr.offset() + u64::from(sector_offset) + (offset >> 9) as u64);
            // SAFETY: the block's buffer, which the round has until its wait,
            // after this bio's endio
            unsafe { bio.bio_mut().map(base.wrapping_add(offset), b) };
            bio.submit::<BlockEndio>();
            offset += b;
        }
    }

    fn record(buf: &mut StripeBuf, errs: BlockErrs) {
        for (i, e) in errs.0.into_iter().enumerate() {
            if e.is_some() {
                buf.err[PRE][i] = e;
            }
        }
    }

    /// Every bio done, and each block's error in the buffer.
    pub(crate) fn wait_sync(self: Pin<&mut Self>) {
        let this = self.project();
        let errs = this.done.wait_sync();
        Self::record(this.buf, errs);
    }

    /// As wait_sync(), awaited: resumed @at.
    pub(crate) async fn wait(self: Pin<&mut Self>, at: Resume<'_>) {
        let this = self.project();
        let errs = this.done.wait().resume_on(at).await;
        Self::record(this.buf, errs);
    }
}

/// The blocks of @s in @blocks read into @buf, waited on.
pub(crate) fn read_sync(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, buf: &mut StripeBuf, blocks: u32) {
    stack_pin_init!(let io = BlockIo::read(fs, buf));
    for i in blocks_in(blocks & ((1u64 << s.v().nr_blocks) - 1) as u32) {
        io.as_mut().submit(s, i);
    }
    io.as_mut().wait_sync();
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
/// ever reads them: bch2_ec_scrub_block(). The error if the block couldn't
/// be read, including its pointer going stale: the stripe deleted, or
/// reused, under us. Its buffer, of one granule, is its own.
pub fn scrub_block(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, block: usize) -> Result<Scrubbed, BchError> {
    let sh = shape(s);
    let mut ret = Scrubbed { bad_sectors: 0, want: Default::default(), got: Default::default() };
    if sh.csum_type == 0 {
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

        let want = s.v().csums().ok_or_else(|| fs.err(bch_errcode::BCH_ERR_stripe_unknown_csum_type))?
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
            ca.io_error(c::bch_member_error_type::BCH_MEMBER_ERROR_checksum);
        }
    }
    Ok(ret)
}

// ── The read path's reconstruct ──────────────────────────────────────────

/// @rbio's extent read from its stripe instead - reconstructed from the rest
/// of the stripe, its block having failed: bch2_ec_read_extent(). What went
/// wrong into @msg, for the read's error message.
///
/// The stripe's pointers are checked for staleness while the btree locks are
/// still held, the stripe key just read under them: a live stripe pointing
/// into a reused bucket is an allocator inconsistency, open or not. After the
/// IO, validate() checks again, and there the stripe being open is what makes
/// a stale pointer one - the key may have been deleted while the IO was in
/// flight.
fn read_extent(trans: &BtreeTrans<'_>, rbio: &mut c::bch_read_bio, msg: &mut Printbuf) -> Result<(), BchError> {
    // the extent we're reading for still locked, for the check below
    trans.relock()?;
    let fs = trans.fs();
    assert!(rbio.pick.has_ec, "reconstructing a read of an extent with no stripe");

    let mut key = BkeyBuf::new();
    let idx = rbio.pick.ec.idx();
    let found = lockrestart_do(trans, |t| {
        let mut iter = BtreeIter::new(t, c::btree_id::stripes, pos(0, idx), BtreeIterFlags::SLOTS);
        let k = iter.peek_slot(t)?.expect("a slot always has a key");
        Ok(k.arm::<c::Stripe>().map(|s| key.reassemble(s.untyped())).is_some())
    });
    if !matches!(found, Ok(true)) {
        prt!(msg, "error looking up stripe\n");
        return fs.throw(bch_errcode::BCH_ERR_stripe_reconstruct);
    }

    let s = BkeySC::from(&key).arm::<c::Stripe>().expect("a stripe, as looked up");
    if !ptr_matches_stripe(s, &rbio.pick) {
        prt!(msg, "pointer doesn't match stripe\n{}\n", s.to_text(fs));
        return fs.throw(bch_errcode::BCH_ERR_stripe_reconstruct);
    }

    let block = usize::from(rbio.pick.ec.block());
    let sectors = rbio.bio.bi_iter.bi_size >> 9;
    let offset = (rbio.bio.bi_iter.bi_sector - s.v().block_ptrs()[block].offset()) as u32;
    if offset + sectors > u32::from(s.v().sectors.get()) {
        prt!(msg, "read is bigger than stripe\n{}\n", s.to_text(fs));
        return fs.throw(bch_errcode::BCH_ERR_stripe_reconstruct);
    }

    let mut have_stale = false;
    let mut stale = c::StripeBlockMask::default();
    {
        let rcu = read_lock();
        for (i, p) in s.v().block_ptrs().iter().enumerate() {
            if fs.dev_rcu(p.dev().into(), &rcu).is_some_and(|ca| ca.ptr_stale(p, &rcu).is_none_or(|n| n != 0)) {
                stale.set(i);
                have_stale = true;
            }
        }
    }

    if have_stale {
        let mut m = log_msg_ratelimited!(fs);
        prt!(&mut m, "Stripe with stale pointer(s):\n{}", s.to_text(fs));
        count_fsck_err(fs, id::stale_dirty_ptr, &mut m);
        let _ = passes::run_explicit(fs, &mut m, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_allocations,
                                     c::bch_run_recovery_pass_flags::from_bits_retain(0));
    }

    // no btree locks held for stripe buffer allocations, or IO
    trans.unlock();

    let Ok(mut buf) = StripeBuf::new(fs, s, EC_BLOCKS_ALL, offset, sectors, MemWait::Never) else {
        prt!(msg, "error allocating stripe data buffers\n{}\n", s.to_text(fs));
        return fs.throw(bch_errcode::BCH_ERR_stripe_reconstruct_enomem);
    };
    buf.stale = stale;

    read_sync(fs, s, &mut buf, EC_BLOCKS_ALL);

    // A block went stale under us: the stripe was deleted or replaced, so
    // the extent has changed. Retry, as a direct read of a stale pointer is.
    let ret = validate(fs, s, &mut buf, false, EC_BLOCKS_ALL);
    if ret.is_err_and(|e| e == BchError::from(bch_errcode::BCH_ERR_stripe_reconstruct_stale_race)) {
        return fs.throw(if rbio.flags & c::bch_read_flags::BCH_READ_retry_if_stale.bits() as u16 != 0 {
            bch_errcode::BCH_ERR_data_read_ptr_stale_retry
        } else {
            bch_errcode::BCH_ERR_data_read_ptr_stale_race
        });
    }

    if ret.is_ok() {
        let from = ((offset - buf.offset) as usize) << 9;
        let src = &buf.block(block)[from..from + ((sectors as usize) << 9)];
        // SAFETY: the read's bio, where its data goes
        unsafe { Bio::from_raw(&raw mut rbio.bio) }.range().copy_from(src);
    }

    if ret.is_ok() && errs_only_dev_offline(&buf) {
        return Ok(());
    }

    errs_to_text(msg, fs, s, &buf, false, EC_BLOCKS_ALL);

    if buf.nr_failed(PRE) != 0 || buf.nr_failed(POST) != 0 {
        match ret {
            Err(e) => prt!(msg, "error: {e}\n"),
            Ok(()) => prt!(msg, "successful reconstruct\n"),
        }
    }
    ret
}

/// # Safety
/// @trans not in an attempt; @rbio and @msg the caller's.
#[c_export]
pub unsafe fn bch2_ec_read_extent(trans: &BtreeTrans<'_>, rbio: &mut c::bch_read_bio, _orig_k: BkeySC<'_>,
                                  msg: &mut Printbuf) -> c_int {
    ret_to_c(read_extent(trans, rbio, msg))
}

/// scrub_block(), for move.c's scrub: the bad sectors, the first bad
/// granule's checksums into @want and @got - or the error.
#[c_export]
pub fn bch2_ec_scrub_block(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, block: c_uint, want: &mut c::bch_csum,
                           got: &mut c::bch_csum) -> i64 {
    let s = k.arm::<c::Stripe>().expect("a stripe");
    match scrub_block(Fs::from_c(c), s, block as usize) {
        Ok(r) => {
            (*want, *got) = (r.want, r.got);
            r.bad_sectors as i64
        }
        Err(e) => i64::from(ret_to_c(Err(e))),
    }
}
