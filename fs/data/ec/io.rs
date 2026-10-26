// SPDX-License-Identifier: GPL-2.0

//! Stripes in memory - ec/io.c's: their block IO, their parity, generated and
//! used to reconstruct, their checksums, and validating what a read brought
//! back.
//!
//! A stripe in memory is two things, apart: its key, and a buffer.
//!
//! The key is whatever holds it - a BkeyBuf a reader copied it into, or for a
//! stripe being made, a StripeKey: a key with room for all of its value,
//! edited as its blocks are allocated. What only reads a key takes it
//! borrowed, as a BkeySC.
//!
//! A buffer (StripeBuf) is a window of some of the stripe's blocks - @size
//! sectors of each from @offset, rounded out to checksum granules - in memory
//! it charges against ec_stripe_buf_limit, and gives back when it's dropped;
//! data[i] is block i's. It's made for a key, and remembers enough of it -
//! how many blocks - to check it's handed that key again. A read records
//! each block's error in err[PRE]; reconstructing the failed blocks from the
//! rest, and checking them, records what's still wrong in err[POST]. A block
//! with an error is an erasure, whatever the error: a checksum mismatch
//! counts as much as a failed read.
//!
//! Parity is raid5's P and raid6's Q, by util/raid.rs, over a buffer's
//! blocks as slices: nr_redundant is at most 2.

use crate::btree::bkey::{pos, BkeyI, BkeySC, BkeyV};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans};
use crate::c;
use crate::c::bch_errcode as err;
use crate::data::ec::trigger::ptr_matches_stripe;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, id};
use crate::init::passes;
use crate::sb::members::{DevIoRef, IoRefIdx};
use crate::util::alloc::{ABox, AVec, AllocError, ConstAlign};
use crate::util::backref::BackRef;
use crate::util::bio::{Bio, BioBox, BioOwner, Endio, RawBio};
use crate::util::closure::{closure_wait, closure_wake_up};
use crate::util::ffi::Opaque;
use crate::util::gather::{Fold, Gather, GatherRef};
use crate::util::kernel::{local_clock, PAGE_SIZE, WRITE};
use crate::util::log::{LogLevel, LogMsg, Ratelimit};
use crate::util::raid;
use crate::util::rcu::read_lock;
use crate::util::sleep::Sleep;
use crate::util::task::{Resumable, Resume};
use crate::util::vstructs::{cast_mut, cast_plain, cast_plain_mut, plain_bytes, plain_bytes_mut, Validated};
use crate::util::Printbuf;
use crate::{c_function_name, log_msg_ratelimited, prt};
use arrayvec::ArrayVec;
use core::ffi::{c_int, c_uint, CStr};
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI32, Ordering};
use cstruct_macros::c_export;
use pin_init::{pin_data, pin_init, stack_pin_init, PinInit};

/// A buffer's errors: as read, and after reconstruction.
pub(crate) const PRE: usize = 0;
pub(crate) const POST: usize = 1;

/// Every block has to come back good - io.h's EC_BLOCKS_ALL: the most a read's
/// @required can say.
pub(crate) const EC_BLOCKS_ALL: u32 = !0;

// ── A stripe's key ───────────────────────────────────────────────────────

/// The stripe's shape, copied out of its header.
#[derive(Clone, Copy)]
struct Shape {
    nr_blocks:             u32,
    nr_redundant:          u32,
    /// unsigned, as C's - nr_redundant past nr_blocks is damage
    nr_data:               u32,
    sectors:               u32,
    csum_type:             u8,
    csum_granularity_bits: u8,
    csums_per_device:      u32,
}

fn shape(s: BkeySC<'_, c::Stripe<Validated>>) -> Shape {
    let v = s.v();
    Shape {
        nr_blocks:             v.nr_blocks.into(),
        nr_redundant:          v.nr_redundant.into(),
        nr_data:               v.nr_data(),
        sectors:               v.sectors.get().into(),
        csum_type:             v.csum_type,
        csum_granularity_bits: v.csum_granularity_bits,
        csums_per_device:      v.csums_per_device(),
    }
}

/// A stripe's key with room for all of its value - as much as any key holds,
/// its u64s being a u8: the key of a stripe being made, edited as its blocks
/// are allocated. Zeroed, it's a deleted key, no stripe.
#[repr(C)]
#[derive(Clone)]
pub struct StripeKey([u64; c::BKEY_U64s_MAX as usize]);

// SAFETY: u64s
unsafe impl ::pin_init::Zeroable for StripeKey {}

impl StripeKey {
    fn room(&self) -> &[u8] {
        plain_bytes(&self.0)
    }

    fn room_mut(&mut self) -> &mut [u8] {
        plain_bytes_mut(&mut self.0)
    }

    /// Its stripe: the key, as long as its u64s say.
    pub(crate) fn stripe(&self) -> BkeySC<'_, c::Stripe<Validated>> {
        let room = self.room();
        let len = cast_plain::<c::bkey>(&room[..size_of::<c::bkey>()]).expect("a key's header").bytes();
        assert!(len <= room.len(), "a stripe key of {len} bytes, in {} of room", room.len());

        // SAFETY: a key, all its u64s in @room - which we have borrowed
        let k: &BkeyI = unsafe { &*BkeyI::from_raw(room.as_ptr().cast()) };
        BkeySC::from(k).arm::<c::Stripe>().expect("a StripeKey's key is a stripe")
    }

    /// Its slot: the stripe's index.
    pub(crate) fn idx(&self) -> u64 {
        self.stripe().k.p.offset
    }

    /// Make it a copy of @k, in place: it's too big to pass by value.
    pub(crate) fn copy_from(&mut self, k: &StripeKey) {
        self.0 = k.0;
    }

    /// Make it @k: bkey_reassemble().
    pub fn set(&mut self, k: BkeySC<'_, c::Stripe<Validated>>) {
        let (hdr, val) = (size_of::<c::bkey>(), k.val_bytes());
        let room = self.room_mut();
        assert!(hdr + val.len() <= room.len(), "a stripe of {} bytes, too big for a key", val.len());

        room[..hdr].copy_from_slice(plain_bytes(k.k));
        room[hdr..hdr + val.len()].copy_from_slice(val);
    }

    /// Make it a new stripe, @v its header: bkey_stripe_init(), and the value
    /// as long as @v says. Past the header it's as it was - zeroed, for a new
    /// stripe.
    pub(crate) fn init(&mut self, v: &c::Stripe) {
        let mut k = c::bkey::new();
        k.type_ = c::bch_bkey_type::KEY_TYPE_stripe.0 as u8;
        k.set_val_bytes(v.val_u64s() as usize * size_of::<u64>());

        let (hdr, room) = (size_of::<c::bkey>(), self.room_mut());
        assert!(k.bytes() <= room.len(), "a new stripe of {} bytes, too big for a key", k.bytes());
        room[..hdr].copy_from_slice(plain_bytes(&k));
        room[hdr..hdr + size_of::<c::Stripe>()].copy_from_slice(plain_bytes(v));
    }

    /// Its header, to edit: its position, say.
    pub(crate) fn k_mut(&mut self) -> &mut c::bkey {
        cast_plain_mut(&mut self.room_mut()[..size_of::<c::bkey>()]).expect("a key's header")
    }

    /// Its stripe's value, to edit.
    pub(crate) fn stripe_mut(&mut self) -> &mut c::Stripe<Validated> {
        let (hdr, len) = (size_of::<c::bkey>(), self.stripe().val_bytes().len());
        cast_mut(&mut self.room_mut()[hdr..hdr + len]).expect("a stripe's value")
    }
}

// ── The buffer's memory ──────────────────────────────────────────────────
//
// Stripe buffers are big - a block's worth for each of the stripe's devices -
// so the memory they hold is counted, ec.stripe_buf_bytes, against
// ec_stripe_buf_limit, a percentage of RAM. A buffer that would go over waits,
// if it can, for others to be freed - unless nothing else is counted: then
// it's let in, or a stripe bigger than the limit would never be made. Who
// waits is the write path's closures, on ec.stripe_buf_wait, until it
// converts, and Rust's, on ec.stripe_buf_freed; freeing wakes both.
//
// IO doesn't outlive a buffer here: a round of IO has the buffer by &mut until
// it's waited on (BlockIo), so freeing it waits for nothing - where C's exit
// waited on the buffer's closure.

/// How a stripe buffer over the limit waits.
pub(crate) enum MemWait {
    /// It doesn't: it's let in - a read reconstructing, which can't wait.
    Never,
    /// The write path's closure, put on the waitlist: stripe_buf_mem_blocked,
    /// and the write waits on its closure.
    Closure(NonNull<c::closure>),
    /// stripe_buf_mem_blocked, for the caller to drop its locks,
    /// stripe_buf_mem_wait() and try again.
    Blocked,
}

impl Fs {
    fn stripe_buf_limit(&self) -> usize {
        crate::util::kernel::totalram_bytes() / 100 * usize::from(self.opts().ec_stripe_buf_limit)
    }

    /// @bytes of stripe buffer memory, counted - unless that would go over
    /// the limit, and @wait waits: stripe_buf_mem_blocked.
    fn stripe_buf_mem_get(&self, bytes: usize, wait: MemWait) -> Result<(), BchError> {
        let limit = self.stripe_buf_limit();
        let mut in_use = self.ec.stripe_buf_bytes.lock();

        if !matches!(wait, MemWait::Never) && *in_use != 0 && *in_use + bytes > limit {
            if let MemWait::Closure(cl) = wait {
                // SAFETY: the write path's closure, waiting on nothing else -
                // or already here, as C's closure_wait() allows
                unsafe { closure_wait(&self.ec.stripe_buf_wait, cl) };
            }
            return self.throw(err::BCH_ERR_stripe_buf_mem_blocked);
        }
        *in_use += bytes;
        Ok(())
    }

    /// @bytes of stripe buffer memory given back, and its waiters woken.
    fn stripe_buf_mem_put(&self, bytes: usize) {
        {
            let mut in_use = self.ec.stripe_buf_bytes.lock();
            *in_use -= bytes;
            // as C's, under the lock
            closure_wake_up(&self.ec.stripe_buf_wait);
        }
        self.ec.stripe_buf_freed.wake();
    }

    /// Sleep until a buffer of @bytes would fit - MemWait::Blocked's wait, with
    /// no btree locks held.
    pub(crate) fn stripe_buf_mem_wait(&self, bytes: usize) {
        let limit = self.stripe_buf_limit();
        let Ok(()) = self.ec.stripe_buf_freed.wait_until(Sleep::UNINTERRUPTIBLE, || {
            let in_use = *self.ec.stripe_buf_bytes.lock();
            (in_use == 0 || in_use + bytes <= limit).then_some(())
        });
    }

    /// Stripe buffer memory in use, and the limit: for to_text.
    pub(crate) fn stripe_buf_mem(&self) -> (usize, usize) {
        (*self.ec.stripe_buf_bytes.lock(), self.stripe_buf_limit())
    }
}

/// Its IO doesn't outlive it - a round has it by &mut until it's waited on -
/// so dropping it waits for nothing.
pub struct StripeBuf {
    /// The filesystem whose memory it holds, which outlives it: not a
    /// borrow, as a stripe being made - which the filesystem owns - holds one
    fs:         BackRef<Fs>,
    nr_blocks:  u32,
    /// The blocks it has a buffer for, and each one's bytes
    blocks:     u32,
    bytes:      usize,
    /// Its window: @size sectors from @offset, into the stripe
    pub(crate) offset: u32,
    pub(crate) size:   u32,
    pub(crate) err:    [[Option<BchError>; MAX_BLOCKS]; 2],
    /// Each block's buffer, of @bytes
    pub(crate) data:   [Option<BlockBuf>; MAX_BLOCKS],
    /// Stale when we read the stripe key: an alloc inconsistency
    pub(crate) stale:  c::StripeBlockMask,
    pub(crate) csum_good: [c::bch_csum; MAX_BLOCKS],
    pub(crate) csum_bad:  [c::bch_csum; MAX_BLOCKS],
}

/// A block's buffer: page aligned, for its IO.
pub(crate) type BlockBuf = ABox<[u8], ConstAlign<PAGE_SIZE>>;

/// A block's buffer of @bytes, not zeroed.
fn block_buf(bytes: usize) -> Result<BlockBuf, AllocError> {
    let mut v = AVec::<u8, ConstAlign<PAGE_SIZE>>::new(PAGE_SIZE);
    v.try_reserve_exact(bytes).map_err(|_| AllocError)?;
    // SAFETY: within its capacity. Its bytes are as kvmalloc() left them,
    // as C's buffers were: read into, or written, before they're read
    unsafe { v.set_len(bytes) };
    Ok(v.into_boxed_slice())
}

impl StripeBuf {
    /// Buffers for stripe @s's blocks in @blocks, @size sectors from @offset -
    /// rounded out to checksum granules, and to the stripe - their memory
    /// counted, which @wait says how to wait for.
    pub(crate) fn new(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, blocks: u32, offset: u32, size: u32,
                      wait: MemWait) -> Result<Self, BchError> {
        let sh = shape(s);
        let granularity = 1u32 << sh.csum_granularity_bits;
        let end = offset + size;
        assert!(end <= sh.sectors, "a stripe buffer of {offset}+{size} sectors, of a stripe of {}", sh.sectors);

        let blocks = blocks & ((1u64 << sh.nr_blocks) - 1) as u32;
        let offset = offset & !(granularity - 1);
        let end = sh.sectors.min(end.next_multiple_of(granularity));
        let bytes = ((end - offset) as usize) << 9;
        fs.stripe_buf_mem_get(bytes * blocks.count_ones() as usize, wait)?;

        // charged for all of @blocks: a drop on the way out frees what it got
        let mut buf = StripeBuf {
            // SAFETY: a buffer is a local of code that has @fs, or a stripe
            // being made's, which new_stripes owns
            fs:        unsafe { BackRef::new(fs) },
            nr_blocks: sh.nr_blocks,
            blocks,
            bytes,
            offset,
            size:      end - offset,
            err:       [[None; MAX_BLOCKS]; 2],
            data:      [const { None }; MAX_BLOCKS],
            stale:     Default::default(),
            csum_good: [c::bch_csum::default(); MAX_BLOCKS],
            csum_bad:  [c::bch_csum::default(); MAX_BLOCKS],
        };
        for i in blocks_in(blocks) {
            match block_buf(bytes) {
                Ok(b) => buf.data[i] = Some(b),
                Err(_) => return fs.throw(err::BCH_ERR_ENOMEM_stripe_buf),
            }
        }
        Ok(buf)
    }

    /// Every block of @s, all of it.
    pub(crate) fn new_whole(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, wait: MemWait) -> Result<Self, BchError> {
        let sectors = s.v().sectors.get().into();
        Self::new(fs, s, EC_BLOCKS_ALL, 0, sectors, wait)
    }

    /// The window moved to the granule at @offset, for scrub's walk along a
    /// block: as many sectors as the buffer was made for, or as the stripe
    /// has left. Its errors are cleared.
    pub(crate) fn slide(&mut self, s: BkeySC<'_, c::Stripe<Validated>>, offset: u32) {
        let sh = self.shape(s);
        let granularity = 1u32 << sh.csum_granularity_bits;
        assert!(offset & (granularity - 1) == 0 && offset < sh.sectors,
                "a stripe buffer slid to {offset}, of a stripe of {} sectors", sh.sectors);

        self.offset = offset;
        self.size   = ((self.bytes >> 9) as u32).min(sh.sectors - offset);
        self.err    = [[None; MAX_BLOCKS]; 2];
    }

    /// @s's shape, checked against what the buffer was made for.
    fn shape(&self, s: BkeySC<'_, c::Stripe<Validated>>) -> Shape {
        let sh = shape(s);
        assert_eq!(sh.nr_blocks, self.nr_blocks, "a stripe buffer, handed a stripe of another width");
        sh
    }

    /// Whether it has block @i.
    pub(crate) fn has(&self, i: usize) -> bool {
        self.blocks & 1 << i != 0
    }

    /// The blocks with an error @e - pre or post recovery - as a mask.
    pub(crate) fn failed_mask(&self, e: usize) -> u32 {
        (0..self.nr_blocks as usize)
            .filter(|&i| self.err[e][i].is_some())
            .fold(0, |m, i| m | 1 << i)
    }

    fn nr_failed(&self, e: usize) -> u32 {
        self.failed_mask(e).count_ones()
    }

    /// Block @i's window, @size sectors from @offset: one it has.
    pub(crate) fn block(&self, i: usize) -> &[u8] {
        let b = self.data[i].as_deref().unwrap_or_else(|| panic!("block {i} of a stripe buffer without it"));
        &b[..(self.size as usize) << 9]
    }

    /// block(), to write.
    pub(crate) fn block_mut(&mut self, i: usize) -> &mut [u8] {
        let bytes = (self.size as usize) << 9;
        let b = self.data[i].as_deref_mut().unwrap_or_else(|| panic!("block {i} of a stripe buffer without it"));
        &mut b[..bytes]
    }

    /// Its blocks' windows, in order, each @size sectors: for the parity math.
    fn blocks_mut(&mut self) -> ArrayVec<&mut [u8], MAX_BLOCKS> {
        let bytes = (self.size as usize) << 9;
        self.data.iter_mut().flatten().map(|b| &mut b[..bytes]).collect()
    }
}

impl Drop for StripeBuf {
    /// Its memory given back - its block buffers go with it.
    fn drop(&mut self) {
        self.fs.stripe_buf_mem_put(self.bytes * self.blocks.count_ones() as usize);
    }
}

/// The blocks in @mask, in order.
pub(crate) fn blocks_in(mask: u32) -> impl Iterator<Item = usize> {
    (0..MAX_BLOCKS).filter(move |i| mask & 1 << i != 0)
}

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

const MAX_BLOCKS: usize = c::BCH_BKEY_PTRS_MAX as usize;

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
    /// Its block's device: the bio's io ref on it, read or write as @write -
    /// leaked into it by submit_range(), adopted back by its endio
    ca:          *const c::bch_dev,
    /// A ref on the round's Gather, for the endio to put
    done:        Option<GatherRef<BlockErrs>>,
    /// The block's pointer, for the endio's staleness check
    ptr:         c::bch_extent_ptr,
    submit_time: u64,
    idx:         u8,
    write:       bool,
    #[pin]
    bio:         Bio,
}

crate::has_bio!(EcBio, bio);

// SAFETY: its device is reached by raw pointer from where the IO completes,
// as C's is: its io ref, which it holds
unsafe impl Send for EcBio {}

/// A round's bio done: its IO accounted against the device, and its block's
/// error folded into the round's Gather - the device's, or its pointer gone
/// stale.
struct BlockEndio;

impl Endio<BioBox<EcBio>> for BlockEndio {
    fn endio(mut bio: BioBox<EcBio>) {
        let done = bio.as_mut().project().done.take().expect("a round's bio, with its ref");
        let (ca, ptr, idx, write, submit_time) = (bio.ca, bio.ptr, bio.idx, bio.write, bio.submit_time);
        let status = bio.bio.status();
        drop(bio);

        // SAFETY: the bio's io ref, of @write's kind, leaked into it by
        // submit_range()
        let e = unsafe {
            if write {
                block_done(DevIoRef::<c::bch_dev_write_ref>::adopt(&*ca), &ptr, submit_time, status)
            } else {
                block_done(DevIoRef::<c::bch_dev_read_ref>::adopt(&*ca), &ptr, submit_time, status)
            }
        };
        let mut errs = BlockErrs::default();
        errs.0[usize::from(idx)] = e;
        done.put(errs);
    }
}

/// A bio's IO, on block pointer @ptr, accounted against its device - @ca, its
/// io ref, put as this returns: its error, or its pointer gone stale.
fn block_done<R: IoRefIdx>(ca: DevIoRef<'_, R>, ptr: &c::bch_extent_ptr, submit_time: u64,
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
            Some(ca.fs().err(err::BCH_ERR_stripe_read_ptr_stale)),
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

impl<'a, R: IoRefIdx> BlockIo<'a, R> {
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
            this.buf.err[PRE][idx] = Some(fs.err(err::BCH_ERR_stripe_read_device_offline));
            return;
        };
        if ca.ptr_stale(&ptr, &read_lock()).is_none_or(|n| n != 0) {
            this.buf.err[PRE][idx] = Some(fs.err(err::BCH_ERR_stripe_read_ptr_stale));
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
            let ca_ptr = core::ptr::from_ref::<c::bch_dev>(&ca);
            let mut bio = set.alloc::<BchError, _>(ca.disk_sb.bdev, nr_vecs as u16, opf, |at| pin_init!(EcBio {
                ca: ca_ptr,
                done,
                ptr,
                submit_time: local_clock(),
                idx: idx as u8,
                write,
                bio <- at.init::<BchError>(),
            }? BchError)).expect("a bio from the stripe bioset's mempool");

            bio.bio_mut().set_sector(ptr.offset() + u64::from(sector_offset) + (offset >> 9) as u64);
            // SAFETY: the block's buffer, which the round has until its wait,
            // after this bio's endio
            unsafe { bio.bio_mut().map(base.wrapping_add(offset), b) };
            // the bio's io ref, which its endio adopts
            ca.clone().leak();
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

        let want = s.v().csums().ok_or_else(|| fs.err(err::BCH_ERR_stripe_unknown_csum_type))?
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
        return fs.throw(err::BCH_ERR_stripe_reconstruct);
    }

    let s = BkeySC::from(&key).arm::<c::Stripe>().expect("a stripe, as looked up");
    if !ptr_matches_stripe(s, &rbio.pick) {
        prt!(msg, "pointer doesn't match stripe\n{}\n", s.to_text(fs));
        return fs.throw(err::BCH_ERR_stripe_reconstruct);
    }

    let block = usize::from(rbio.pick.ec.block());
    let sectors = rbio.bio.bi_iter.bi_size >> 9;
    let offset = (rbio.bio.bi_iter.bi_sector - s.v().block_ptrs()[block].offset()) as u32;
    if offset + sectors > u32::from(s.v().sectors.get()) {
        prt!(msg, "read is bigger than stripe\n{}\n", s.to_text(fs));
        return fs.throw(err::BCH_ERR_stripe_reconstruct);
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
        return fs.throw(err::BCH_ERR_stripe_reconstruct_enomem);
    };
    buf.stale = stale;

    read_sync(fs, s, &mut buf, EC_BLOCKS_ALL);

    // A block went stale under us: the stripe was deleted or replaced, so
    // the extent has changed. Retry, as a direct read of a stale pointer is.
    let ret = validate(fs, s, &mut buf, false, EC_BLOCKS_ALL);
    if ret.is_err_and(|e| e == BchError::from(err::BCH_ERR_stripe_reconstruct_stale_race)) {
        return fs.throw(if rbio.flags & c::bch_read_flags::BCH_READ_retry_if_stale.bits() as u16 != 0 {
            err::BCH_ERR_data_read_ptr_stale_retry
        } else {
            err::BCH_ERR_data_read_ptr_stale_race
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

// ── Checksums ────────────────────────────────────────────────────────────

/// Block @block's checksum over the granule at @offset - sectors into the
/// stripe - as far as the buffer goes, or the stripe does.
fn block_checksum(s: BkeySC<'_, c::Stripe<Validated>>, b: &StripeBuf, block: u32, offset: u32) -> c::bch_csum {
    let sh = b.shape(s);
    let granularity = 1u32 << sh.csum_granularity_bits;
    let end = b.offset + b.size;
    let len = granularity.min(end - offset);

    assert!(offset < end && offset >= b.offset && offset & (granularity - 1) == 0,
            "checksum granule at {offset} of a buffer of {}+{}", b.offset, b.size);
    assert!(offset + len == sh.sectors || len & (granularity - 1) == 0,
            "short checksum granule at {offset}, not the stripe's last");

    let start = ((offset - b.offset) << 9) as usize;
    let granule = &b.block(block as usize)[start..start + ((len << 9) as usize)];
    c::bch_csum_type(sh.csum_type.into()).checksum_unkeyed(granule)
}

/// Every block's checksums, into @key, from a buffer of the whole stripe.
pub fn generate_checksums(key: &mut StripeKey, buf: &StripeBuf) {
    let sh = buf.shape(key.stripe());
    if sh.csum_type == 0 {
        return;
    }

    assert!(buf.offset == 0 && buf.size == sh.sectors && buf.blocks.count_ones() == sh.nr_blocks,
            "checksums of a buffer of part of the stripe");

    for i in 0..sh.nr_blocks {
        for j in 0..sh.csums_per_device {
            let csum = block_checksum(key.stripe(), buf, i, j << sh.csum_granularity_bits);
            key.stripe_mut().csums_mut().expect("a new stripe's checksum type").set(i, j, csum);
        }
    }
}

/// The blocks - data only with @data_only - that came back without error @e,
/// checked against their checksums: a mismatch is a checksum error, the
/// device's.
fn validate_checksums(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, buf: &mut StripeBuf, data_only: bool, e: usize) {
    let sh = buf.shape(s);
    if sh.csum_type == 0 {
        return;
    }

    let granularity = 1u32 << sh.csum_granularity_bits;
    let nr = if data_only { sh.nr_data } else { sh.nr_blocks };
    let csums = s.v().csums().expect("checked by validate()");

    for i in 0..nr {
        if buf.err[e][i as usize].is_some() {
            continue;
        }

        let end = buf.offset + buf.size;
        let mut offset = buf.offset;
        while offset < end {
            let want = csums.get(i, offset >> sh.csum_granularity_bits);
            let got = block_checksum(s, buf, i, offset);

            if want != got {
                buf.err[e][i as usize] = Some(fs.err(err::BCH_ERR_stripe_read_csum_err));
                buf.csum_good[i as usize] = want;
                buf.csum_bad[i as usize] = got;

                // Not an error for a device that's gone: the key isn't locked
                // any more
                let dev = s.v().block_ptrs()[i as usize].dev();
                if let Some(ca) = fs.dev_get(dev.into()) {
                    ca.io_error(c::bch_member_error_type::BCH_MEMBER_ERROR_checksum);
                }
                break;
            }
            offset += granularity.min(end - offset);
        }
    }
}

/// Whether the blocks in @mask read without error, and match the stripe's
/// checksums. Records nothing: a failure here is followed by a full read
/// and validate_msg(), which does - and fails a checksum type this version
/// doesn't know.
pub fn blocks_good(s: BkeySC<'_, c::Stripe<Validated>>, buf: &StripeBuf, mask: u32) -> bool {
    let v = s.v();
    let Some(csums) = v.csums() else { return false };
    let granularity = 1u32 << v.csum_granularity_bits;
    let (start, end) = (buf.offset, buf.offset + buf.size);

    (0..u32::from(v.nr_blocks)).filter(|i| mask & 1 << i != 0).all(|i| {
        buf.err[PRE][i as usize].is_none() &&
            (v.csum_type == 0 ||
             (start..end).step_by(granularity as usize).all(|offset| {
                 csums.get(i, offset >> v.csum_granularity_bits) == block_checksum(s, buf, i, offset)
             }))
    })
}

// ── Parity and reconstruction ────────────────────────────────────────────

/// Whether @buf has every block of the stripe it was made for: parity, and
/// reconstruction, are over all of them.
fn has_all(buf: &StripeBuf) -> bool {
    buf.blocks.count_ones() == buf.nr_blocks
}

/// Stripe @s's parity, from its data.
pub fn generate_ec(s: BkeySC<'_, c::Stripe<Validated>>, buf: &mut StripeBuf) {
    let sh = buf.shape(s);
    assert!(has_all(buf) && buf.offset == 0 && buf.size == sh.sectors, "parity of part of a stripe");

    raid::generate(&mut buf.blocks_mut(), sh.nr_redundant as usize)
}

/// The blocks that came back bad, rebuilt from the rest - if any of
/// @required, the blocks the caller will use, is one - and the data blocks
/// checked after.
fn do_recov(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, buf: &mut StripeBuf, required: u32)
    -> Result<(), BchError>
{
    // Nothing the caller wants is bad: nothing to reconstruct. Damage to
    // blocks it isn't going to look at isn't an error - a stripe reuse only
    // carries forward blocks holding live data, and regenerates parity, so a
    // dead block holding nothing mustn't stop it: else the rewrite that would
    // drop that block can never run.
    if buf.failed_mask(PRE) & required == 0 {
        return Ok(());
    }

    let sh = buf.shape(s);
    if buf.nr_failed(PRE) > sh.nr_redundant {
        return fs.throw(err::BCH_ERR_stripe_reconstruct_insufficient_blocks);
    }
    assert!(has_all(buf), "reconstructing from a stripe buffer without every block");

    // The whole erasure list, parity included - reconstruction picks its
    // method by it. A lost P or Q left out doesn't just pick a worse one, it
    // drops the count: two erasures reported as one take the single erasure
    // xor path, which reconstructs through the parity block known bad.
    let failed: ArrayVec<u8, MAX_BLOCKS> = (0..sh.nr_blocks as u8)
        .filter(|&i| buf.err[PRE][usize::from(i)].is_some())
        .collect();

    raid::recover(&mut buf.blocks_mut(), sh.nr_redundant as usize, &failed);

    validate_checksums(fs, s, buf, true, POST);

    if buf.failed_mask(POST) & required != 0 {
        return fs.throw(err::BCH_ERR_stripe_read_csum_err);
    }
    Ok(())
}

// ── Validation ───────────────────────────────────────────────────────────

/// A stale read of a stripe that isn't open is an expected race, not an
/// error.
fn read_maybe_spurious(buf: &StripeBuf, i: usize, e: Option<BchError>, is_open: bool) -> bool {
    e == Some(err::BCH_ERR_stripe_read_ptr_stale.into()) && !buf.stale.test(i) && !is_open
}

/// Whether every error is a device that's offline: a device going offline
/// is reported once, by the device, and every stripe that touches it saying
/// so again is noise.
pub fn errs_only_dev_offline(buf: &StripeBuf) -> bool {
    let offline = BchError::from(err::BCH_ERR_stripe_read_device_offline);
    let nr = buf.nr_blocks as usize;
    buf.err.iter().all(|e| e[..nr].iter().all(|e| e.is_none_or(|e| e == offline)))
}

fn errs_to_text_one(out: &mut Printbuf, fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, buf: &StripeBuf, e: usize,
                    is_open: bool, required: u32) {
    let v = s.v();

    for i in 0..usize::from(v.nr_blocks) {
        let Some(error) = buf.err[e][i] else { continue };

        let ca = fs.dev_get(v.block_ptrs()[i].dev().into());
        let name = ca.as_ref().map_or("(invalid device)", |ca| ca.name().to_str().unwrap_or("?"));
        write!(out, "block {i} {name}: {error}");

        if error == err::BCH_ERR_stripe_read_csum_err.into() {
            let ty = c::bch_csum_type(v.csum_type.into());
            write!(out, " expected ");
            buf.csum_good[i].to_text(out, ty);
            write!(out, " got ");
            buf.csum_bad[i].to_text(out, ty);
        }

        if required & 1 << i == 0 {
            write!(out, " (block not in use)");
        }
        if e == PRE && read_maybe_spurious(buf, i, Some(error), is_open) {
            write!(out, " (possibly spurious: stripe not pinned)");
        }
        out.newline();
    }
}

/// Each block's errors, before reconstruction and after.
#[cold]
pub fn errs_to_text(out: &mut Printbuf, fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, buf: &StripeBuf,
                    is_open: bool, required: u32) {
    for (e, what) in [(PRE, "pre"), (POST, "post")] {
        if buf.nr_failed(e) != 0 {
            write!(out, "Errors {what} recovery\n");
            let mut out = out.indent(2);
            errs_to_text_one(&mut out, fs, s, buf, e, is_open, required);
        }
    }
}

/// What a read of the stripe brought back - the read done, the caller's to
/// wait for - checked, and what's bad of @required reconstructed: an error
/// if it couldn't be. A block of a stripe that isn't open (@is_open) going
/// stale is the stripe changing under the read: stripe_reconstruct_stale_race,
/// for the caller to retry.
pub fn validate(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, buf: &mut StripeBuf, is_open: bool, required: u32)
    -> Result<(), BchError>
{
    s.v().csum_type_check(fs)?;
    validate_checksums(fs, s, buf, false, PRE);
    if buf.nr_failed(PRE) == 0 {
        return Ok(());
    }

    let stale = Some(err::BCH_ERR_stripe_read_ptr_stale.into());
    let mut have_stale_race = false;
    for i in 0..usize::from(s.v().nr_blocks) {
        let e = buf.err[PRE][i];
        have_stale_race |= read_maybe_spurious(buf, i, e, is_open);

        // A block of an open stripe can't go stale under us: that's the
        // allocator invalidating a bucket an open stripe references, an
        // inconsistency - counted, so it's seen in the field and fails tests.
        // A device offline or an IO error is the environment, and a stale
        // read of a stripe that isn't open an expected race: neither counts.
        if is_open && e == stale {
            fs.sb_error_count(c::bch_sb_error_id::BCH_FSCK_ERR_stripe_read_ptr_stale);
        }
    }

    match do_recov(fs, s, buf, required) {
        Err(_) if !is_open && have_stale_race => fs.throw(err::BCH_ERR_stripe_reconstruct_stale_race),
        ret => ret,
    }
}

/// validate(), and what went wrong logged, as from @caller - and counted -
/// unless it was nothing, or only devices offline.
pub fn validate_msg(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, buf: &mut StripeBuf, is_open: bool, required: u32,
                    caller: &CStr) -> Result<(), BchError> {
    let ret = validate(fs, s, buf, is_open, required);

    if ret.is_ok() && buf.nr_failed(PRE) == 0 && buf.nr_failed(POST) == 0 {
        return Ok(());
    }
    if ret == Err(err::BCH_ERR_stripe_reconstruct_stale_race.into()) {
        return ret;
    }
    if ret.is_ok() && errs_only_dev_offline(buf) {
        return Ok(());
    }

    // The device's fault and the outcome are counted apart: a checksum error
    // is a live device handing back data that didn't match - how a failing
    // drive is found - and whether reconstruction saved us is another
    // question. Per block, as the read path counts per failure. Devices
    // offline and IO errors aren't counted, as above.
    let csum_err = Some(err::BCH_ERR_stripe_read_csum_err.into());
    let nr = usize::from(s.v().nr_blocks);
    for _ in buf.err[PRE][..nr].iter().filter(|&&e| e == csum_err) {
        fs.sb_error_count(c::bch_sb_error_id::BCH_FSCK_ERR_stripe_read_csum_err);
    }

    // do_recov()'s verdict fails two ways: too many blocks gone to run at
    // all, or running and producing blocks that still don't check out. Both
    // cost the caller its data.
    if ret.is_err() {
        fs.sb_error_count(c::bch_sb_error_id::BCH_FSCK_ERR_stripe_reconstruct_failed);
    }

    // Damage only to blocks the caller isn't using isn't an error - a stripe
    // reuse discards them - but still worth saying, a device producing bad
    // blocks: at notice, and it's the same stripe reporting the same dead
    // block each time it comes off the lru, so with ratelimit state of its
    // own rather than eating the budget of damage that mattered.
    static RATELIMIT_ERR: Ratelimit = Ratelimit::new();
    static RATELIMIT_RECONSTRUCTED: Ratelimit = Ratelimit::new();
    static RATELIMIT_NOTICE: Ratelimit = Ratelimit::new();

    let damage_matters = buf.failed_mask(PRE) & required != 0;
    let (level, ratelimit) = match (&ret, damage_matters) {
        (Err(_), _)   => (LogLevel::ERR, &RATELIMIT_ERR),
        (Ok(()), true)  => (LogLevel::ERR, &RATELIMIT_RECONSTRUCTED),
        (Ok(()), false) => (LogLevel::NOTICE, &RATELIMIT_NOTICE),
    };

    let mut msg = LogMsg::with_level(fs, level);
    write!(msg, "{}(): {}:\n", caller.to_str().unwrap_or("?"),
           if ret.is_err() || damage_matters { "error reading stripe" } else { "damaged stripe blocks, none in use" });
    prt!(&mut msg, "{}\n", s.untyped().to_text(fs));
    errs_to_text(&mut msg, fs, s, buf, is_open, required);

    match &ret {
        Err(e)               => write!(msg, "error: {e}\n"),
        Ok(()) if damage_matters => write!(msg, "successful reconstruct\n"),
        Ok(())               => {}
    }
    msg.set_suppressed(ratelimit.suppress(fs, c_function_name!()));
    ret
}
