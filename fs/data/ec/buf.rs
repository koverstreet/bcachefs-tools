// SPDX-License-Identifier: GPL-2.0

//! Stripe buffers: StripeBuf, its memory, and what's computed over one -
//! checksums, parity, reconstruction, validation.
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

use crate::btree::bkey::{BkeySC, BkeyV};
use crate::c;
use crate::data::ec::stripe::{shape, Shape, StripeKey, MAX_BLOCKS};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::util::{raid, Printbuf};
use crate::util::alloc::{kbox_pin_init, ABox, AVec, AllocError, ConstAlign, KBox};
use crate::util::backref::BackRef;
use crate::util::closure::{closure_wait, closure_wake_up};
use crate::util::kernel::PAGE_SIZE;
use crate::util::log::{LogLevel, LogMsg, Ratelimit};
use crate::util::sleep::Sleep;
use crate::util::vstructs::Validated;
use crate::{c_function_name, prt};
use arrayvec::ArrayVec;
use core::ffi::CStr;
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{init, Init};

/// A buffer's errors: as read, and after reconstruction.
pub(crate) const PRE: usize = 0;
pub(crate) const POST: usize = 1;

/// Every block has to come back good - io.h's EC_BLOCKS_ALL: the most a read's
/// @required can say.
pub(crate) const EC_BLOCKS_ALL: u32 = !0;

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
        crate::util::kernel::totalram_bytes() / 100 * usize::from(self.opts().ec_stripe_buf_limit.get())
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
            return self.throw(bch_errcode::BCH_ERR_stripe_buf_mem_blocked);
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
    ///
    /// Boxed, and made in the box: a buffer is a kilobyte, and moved by value
    /// it's copied through the stack - deep in a write path, say. The box is
    /// only there because pin-init can't initialize an Option in place, and
    /// a stripe being made keeps its buffers in Options; C's ec_stripe_new has
    /// them inline. Drop it when pin-init can.
    pub(crate) fn new(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, blocks: u32, offset: u32, size: u32,
                      wait: MemWait) -> Result<KBox<Self>, BchError> {
        let sh = shape(s);
        let granularity = 1u32 << sh.csum_granularity_bits;
        let end = offset + size;
        assert!(end <= sh.sectors, "a stripe buffer of {offset}+{size} sectors, of a stripe of {}", sh.sectors);

        let blocks = blocks & ((1u64 << sh.nr_blocks) - 1) as u32;
        let offset = offset & !(granularity - 1);
        let end = sh.sectors.min(end.next_multiple_of(granularity));
        let (size, bytes) = (end - offset, ((end - offset) as usize) << 9);

        let init = init!(StripeBuf {
            // SAFETY: a buffer is a local of code that has @fs, or a stripe
            // being made's, which new_stripes owns
            fs:        unsafe { BackRef::new(fs) },
            nr_blocks: sh.nr_blocks,
            // none yet: what drop gives back
            blocks:    0,
            bytes,
            offset,
            size,
            err:       [[None; MAX_BLOCKS]; 2],
            data:      [const { None }; MAX_BLOCKS],
            stale:     Default::default(),
            csum_good: [c::bch_csum::default(); MAX_BLOCKS],
            csum_bad:  [c::bch_csum::default(); MAX_BLOCKS],
        }? BchError).chain(|buf| {
            fs.stripe_buf_mem_get(bytes * blocks.count_ones() as usize, wait)?;
            // charged for all of @blocks: a drop on the way out frees what it
            // got
            buf.blocks = blocks;
            for i in blocks_in(blocks) {
                match block_buf(bytes) {
                    Ok(b) => buf.data[i] = Some(b),
                    Err(_) => return fs.throw(bch_errcode::BCH_ERR_ENOMEM_stripe_buf),
                }
            }
            Ok(())
        });
        // the box's own allocation failing is the one bare ENOMEM
        match kbox_pin_init(init) {
            Ok(buf) => Ok(Pin::into_inner(buf)),
            Err(e) if e == BchError::from(c::ENOMEM) => fs.throw(bch_errcode::BCH_ERR_ENOMEM_stripe_buf),
            Err(e) => Err(e),
        }
    }

    /// Every block of @s, all of it.
    pub(crate) fn new_whole(fs: &Fs, s: BkeySC<'_, c::Stripe<Validated>>, wait: MemWait)
        -> Result<KBox<Self>, BchError>
    {
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
    pub(crate) fn shape(&self, s: BkeySC<'_, c::Stripe<Validated>>) -> Shape {
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

    pub(crate) fn nr_failed(&self, e: usize) -> u32 {
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

// ── Checksums ────────────────────────────────────────────────────────────

/// Block @block's checksum over the granule at @offset - sectors into the
/// stripe - as far as the buffer goes, or the stripe does.
pub(crate) fn block_checksum(s: BkeySC<'_, c::Stripe<Validated>>, b: &StripeBuf, block: u32, offset: u32) -> c::bch_csum {
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
                buf.err[e][i as usize] = Some(fs.err(bch_errcode::BCH_ERR_stripe_read_csum_err));
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
        return fs.throw(bch_errcode::BCH_ERR_stripe_reconstruct_insufficient_blocks);
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
        return fs.throw(bch_errcode::BCH_ERR_stripe_read_csum_err);
    }
    Ok(())
}

// ── Validation ───────────────────────────────────────────────────────────

/// A stale read of a stripe that isn't open is an expected race, not an
/// error.
fn read_maybe_spurious(buf: &StripeBuf, i: usize, e: Option<BchError>, is_open: bool) -> bool {
    e == Some(bch_errcode::BCH_ERR_stripe_read_ptr_stale.into()) && !buf.stale.test(i) && !is_open
}

/// Whether every error is a device that's offline: a device going offline
/// is reported once, by the device, and every stripe that touches it saying
/// so again is noise.
pub fn errs_only_dev_offline(buf: &StripeBuf) -> bool {
    let offline = BchError::from(bch_errcode::BCH_ERR_stripe_read_device_offline);
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

        if error == bch_errcode::BCH_ERR_stripe_read_csum_err.into() {
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

    let stale = Some(bch_errcode::BCH_ERR_stripe_read_ptr_stale.into());
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
        Err(_) if !is_open && have_stale_race => fs.throw(bch_errcode::BCH_ERR_stripe_reconstruct_stale_race),
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
    if ret == Err(bch_errcode::BCH_ERR_stripe_reconstruct_stale_race.into()) {
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
    let csum_err = Some(bch_errcode::BCH_ERR_stripe_read_csum_err.into());
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
