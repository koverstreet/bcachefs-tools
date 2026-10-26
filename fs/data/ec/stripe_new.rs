// SPDX-License-Identifier: GPL-2.0

/* DOC_LATEX(erasure-coding)
 *
 * Erasure coding in bcachefs works by encoding entire buckets in the background,
 * rather than fragmenting foreground writes into stripes.
 *
 * \paragraph{Write path}
 *
 * Foreground writes are replicated normally (e.g.\ two copies with
 * \texttt{data\_replicas=2}). The reconcile thread tracks buckets containing
 * replicated data that are candidates for erasure coding.
 *
 * When enough candidate buckets accumulate, the erasure coding background job:
 *
 * \begin{enumerate}
 * \item Takes a set of data buckets (e.g.\ 5 buckets with unrelated data)
 * \item Allocates parity buckets (e.g.\ 2 buckets for RAID-6 style redundancy)
 * \item Computes Reed-Solomon parity across the data buckets
 * \item Writes the parity to the new buckets
 * \item Updates every extent pointing into the original data buckets:
 *   \begin{itemize}
 *   \item Drops the extra replica pointers
 *   \item Adds pointers to the parity buckets with a flag indicating
 *         reconstruct-read is required
 *   \end{itemize}
 * \end{enumerate}
 *
 * This approach avoids the write hole entirely: parity is computed once for
 * immutable data, and the extent updates are atomic btree operations.
 *
 * \paragraph{Stripe lifetime}
 *
 * Once buckets are grouped into a stripe, none of them can be reused until
 * \emph{all} data in the stripe is dead or moved. Copygc is aware of this
 * constraint and will evacuate entire stripes when they become fragmented,
 * rewriting live data to new buckets so the old stripe can be reclaimed.
 *
 * \paragraph{Read path}
 *
 * When reading an extent with erasure coding pointers, the read path first
 * attempts to read from the data bucket directly. If that fails (device offline,
 * checksum error), it performs a reconstruct read: fetching the surviving data
 * buckets and parity buckets, then using Reed-Solomon to recover the missing
 * data. The stripe-to-bucket mapping is stored in the stripes btree.
 */

//! Stripes being made - the rest of ec/create.c: a new stripe's life, from
//! the stripe head that stages it to its creation, and repair's rebuild,
//! which makes one from the stripe it replaces.
//!
//! A stripe being made is a StripeNew; C has it only by pointer, an open
//! bucket's ->ec, as ec_stripe_new. It's owned boxed - a StripeNewBox,
//! pinned, as its writers' open buckets and its task hold pointers to it: by
//! its head while it's open, then, once it's handed on, by the filesystem
//! until its task frees it - ec.new_stripes, NewStripes, which keeps room for
//! the heads' so handing one on can't fail. A repair's is in new_stripes from
//! the start. It holds its slot and its old stripe open (StripeClaim), and
//! its buckets in the new stripes' bucket table, till it's freed.
//!
//! Its state:
//! - open: its head's h->s, staged under the head's lock - a slot, buckets,
//!   an old stripe to pack it into, its buffer - and handing blocks to
//!   writers. A repair's is open until it's set up.
//! - filling: handed on, by set_pending(); its writers still writing.
//! - in_flight: every bucket back, its task creating it.
//!
//! Its blocks' buckets, a StripeBlock each: a pin, until a writer claims the
//! block and takes it - then an EcOpenBucket, the bucket's ->ec, which keeps
//! it open past the writer's put. Given back when they're dropped: by
//! teardown, its writers done, or with it.
//!
//! Its io refs: one per block a writer has claimed, put as the writer's open
//! bucket closes - foreground.c's __bch2_open_bucket_put() - and one of its
//! own, its head's or repair's, put by set_pending(). The last put takes it
//! in flight, there and then - its seq assigned, the flushes woken - as C's
//! did: a flush must see a stripe in flight as soon as its writers are done,
//! not when its task next runs. It then puts the one ref on @in_flight, the
//! Gather its task waits on.
//!
//! Its task, one: made with it, unstarted, so starting it can't fail - and
//! started when it's staged, or when it's handed on before. It folds the
//! old stripe's carried blocks in, if it's reading one; waits to be in
//! flight; creates the stripe; and tears it down, freeing it. Every await
//! resumes on stripe_create_wq. That's what makes its borrow of the
//! filesystem sound: going read-only has the heads hand theirs on, and waits
//! for new_stripes to empty and for the task's write ref, which it puts
//! last; and stripe_create_wq, a Workqueue, is destroyed as the filesystem's
//! freed, which drains the task's last run - so what the task does after
//! freeing its stripe still has a filesystem. (C's fold ran on
//! system_dfl_wq, as create waited for it on stripe_create_wq and would
//! have waited on itself; here the fold is the same task, which waits by
//! awaiting, holding no worker.)
//!
//! Who touches it, and how: a StripeNew is its async state - io refs and the
//! Gather, @seq, @state, @err, and @idx, its slot from launch - atomics,
//! anyone's; and its internal state, Internal, under its lock (lock()).
//! Everyone has it by &: its head while it's open, its writers through their
//! open buckets' ->ec, cancelling (@err), its task, to_text.
//!
//! - The head stages it under the head's lock and its own - holding its own
//!   across the allocation's transactions, before launch, when no writer can
//!   have a block - and claims blocks for writers under both.
//! - Writers take it briefly, for their block's buffer - under their write
//!   point's lock - and the allocator stopping a device, for its blocks. An
//!   open bucket's to_text, under spinlocks, reads @idx and takes nothing.
//! - Its task takes it in short sections, never across an await - a mutex
//!   is released by the task that took it, and the task resumes on any
//!   worker: what it reads and writes with - the old stripe's buffer for the
//!   fold, the buffers and keys for create - is taken out, and put back
//!   after. It's kilobytes, so it's taken in place: into Working, in
//!   stripe_task()'s own future, which spawning makes in the task's
//!   allocation - keys copied, buffers swapped. Not in the fold's or
//!   create's futures, which they borrow it from: an awaited future is made
//!   on the worker's stack and moved into its awaiter. The fold's carry
//!   swaps the old buffer's blocks into the new buffer's carried slots under
//!   it, beside writers reading theirs: no writer is ever given a carried
//!   block.
//!
//! The order: a write point's lock, the head's, the stripe's, then btree
//! locks - writers have dropped theirs before they ask for their buffer; and
//! new_stripes' before the stripe's, as to_text takes them, so nothing takes
//! new_stripes' holding the stripe's.

use crate::alloc::buckets::{bucket_to_u64, DiskReservation};
use crate::alloc::disk_groups::Target;
use crate::alloc::foreground::{EcOpenBucket, OpenBucket, OpenBucketRef};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeySC};
use crate::btree::iter::{commit_do, lockrestart_do, trans_commit_do, BtreeIter, BtreeIterFlags, BtreeTrans,
                         CommitFlags, CommitOpts, TransAttempt, UpdateTriggerFlags, Watermark};
use crate::c;
use crate::c::bch_errcode;
use crate::data::ec::create::{create_watermark, dev_stripe_state_get, init_new_stripe_from_old, new_stripe_alloc_buckets,
                              new_stripe_key, record_lost_blocks, stripe_alloc_or_reuse, stripe_delete, stripe_head_get,
                              stripe_head_put, stripe_idx_alloc, stripe_update_extents, HeadGuard};
use crate::data::ec::io::{blocks_good, blocks_in, generate_checksums, generate_ec, read_sync, validate_msg, BlockIo,
                          MemWait, StripeBuf, StripeKey, POST, PRE};
use crate::data::ec::trigger::{OpenStripe, StripeClaim};
use crate::data::moving::{Moving, MovingWrite};
use crate::errcode::{err_ptr, ret_to_c, BchError};
use crate::fs::Fs;
use crate::inode_opts;
use crate::locking::{Guard, Lock};
use crate::logged_ops;
use crate::new_lock;
use crate::sb::counters::events;
use crate::sb::members::DevIoRef;
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::alloc::{kbox_pin_init, AllocError, KBox, KVVec};
use crate::util::backref::BackRef;
use crate::util::closure::StackClosure;
use crate::util::ffi::Opaque;
use crate::util::gather::{Gather, GatherRef};
use crate::util::kernel::local_clock;
use crate::util::rcu::read_lock;
use crate::util::refcount::Refcount;
use crate::util::sleep::Sleep;
use crate::util::task::{spawn_unstarted, Resumable, Resume, Spawn, Unstarted};
use crate::util::vstructs::Validated;
use crate::util::Printbuf;
use crate::{bch_err, btree_trans, fs_inconsistent, log_msg_ratelimited, prt};
use arrayvec::ArrayVec;
use core::ffi::{c_int, c_uint, c_void};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use cstruct_macros::c_export;
use pin_init::{init, init_zeroed, pin_data, pin_init, pinned_drop, stack_pin_init, Init, PinInit};

const MAX_BLOCKS: usize = c::BCH_BKEY_PTRS_MAX as usize;

use c::ec_stripe_new_state as State;

// ── A stripe being made ──────────────────────────────────────────────────

#[pin_data(PinnedDrop)]
pub struct StripeNew {
    /// The filesystem that owns it, which outlives it
    fs:                   BackRef<Fs>,
    /* seq is only assigned once the refs are gone, so it can't give an age */
    start_time:           u64,
    /// Its slot, from launch: for what can't take its lock - an open bucket's
    /// to_text, under spinlocks
    idx:                  AtomicU64,

    io_refs:              AtomicI32,
    /// What its task waits on to be in flight: one ref, held while @io_refs
    /// isn't zero
    #[pin]
    in_flight:            Gather<()>,
    seq:                  AtomicU64,
    /// Why it's cancelled: an errcode, negative, as C's
    pub(crate) err:       AtomicI32,
    /// An ec_stripe_new_state
    state:                AtomicU32,

    #[pin]
    internal:             Lock<c::mutex, Internal>,
}

/// What a stripe being made holds on one of its blocks' buckets: a pin, or -
/// once a writer's claimed the block - the bucket's ->ec. Given back when
/// it's dropped: the pin put, the claimed bucket closed.
pub(crate) enum StripeBlock {
    Pinned(OpenBucket<'static>),
    Claimed(EcOpenBucket<'static>),
}

impl StripeBlock {
    /// For the stripe being made whose filesystem @fs is.
    ///
    /// # Safety
    /// It goes in a stripe being made's blocks[]: the filesystem drains them
    /// before it goes.
    pub(crate) unsafe fn new(ob: OpenBucket<'_>, fs: &BackRef<Fs>) -> Self {
        // SAFETY: the caller's
        StripeBlock::Pinned(unsafe { ob.bind(fs) })
    }
}

impl core::ops::Deref for StripeBlock {
    type Target = OpenBucketRef;

    fn deref(&self) -> &OpenBucketRef {
        match self {
            StripeBlock::Pinned(ob)  => ob,
            StripeBlock::Claimed(ob) => ob,
        }
    }
}

#[pin_data]
pub(crate) struct Internal {
    /// A repair's: its writes, in flight in the repair's moving context
    write:                Option<MovingWrite>,
    /// Its task, until it's started
    task:                 Option<Unstarted<()>>,

    pub(crate) devs:      c::bch_devs_mask,
    pub(crate) watermark: c::bch_watermark,

    pub(crate) have_old_stripe:     bool,
    pub(crate) allocated:           bool,
    /// The old stripe's buffer's allocated: its task reads it
    old_stripe_read:                bool,
    /// ... and every block's in it: there's nothing for the task to read
    old_stripe_read_all:            bool,

    pub(crate) blocks_gotten:    c::StripeBlockMask,
    pub(crate) blocks_allocated: c::StripeBlockMask,
    pub(crate) blocks_moving:    c::StripeBlockMask,
    pub(crate) blocks:           [Option<StripeBlock>; MAX_BLOCKS],
    /// Its parity's, once reserve_parity()'s made it
    pub(crate) res:              Option<DiskReservation<'static>>,

    /// Its buckets in ec.stripes_new_buckets: buckets_add()'s
    buckets:                     ArrayVec<u64, MAX_BLOCKS>,

    pub(crate) new_stripe:       StripeKey,
    pub(crate) new_buf:          Option<StripeBuf>,
    pub(crate) old_stripe:       StripeKey,
    old_buf:                     Option<StripeBuf>,

    pub(crate) new_stripe_open:  Option<StripeClaim>,
    pub(crate) old_stripe_open:  Option<StripeClaim>,

    pub(crate) old_block_map:    [u8; MAX_BLOCKS],
    pub(crate) old_blocks_nr:    u8,
}

/// A stripe being made, owned: pinned, as its task, its writers' open buckets
/// and its blocks point into it.
pub(crate) type StripeNewBox = Pin<KBox<StripeNew>>;

/// The stripes being made that their heads have handed on, and repairs': in
/// bch_fs_ec.new_stripes, each until its task is done with it. A head's
/// stripe is its head's until it's handed on, and comes here then - where
/// nothing can fail - so room is kept for every stripe a head holds:
/// capacity >= len + @heads_held.
pub(crate) struct NewStripes {
    v:          KVVec<StripeNewBox>,
    heads_held: usize,
}

impl NewStripes {
    pub(crate) fn new() -> Self {
        NewStripes { v: KVVec::new(), heads_held: 0 }
    }

    /// Room for one more stripe a head holds.
    fn reserve_for_head(&mut self) -> Result<(), AllocError> {
        self.v.reserve(self.heads_held + 1, GFP_KERNEL)?;
        self.heads_held += 1;
        Ok(())
    }

    /// A head's stripe is gone without being handed on: its room is no
    /// longer kept.
    fn release_for_head(&mut self) {
        self.heads_held = self.heads_held.checked_sub(1).expect("a head's stripe, room kept for it");
    }

    /// A head's stripe, handed on: into the room kept for it, so this can't
    /// fail.
    fn push_from_head(&mut self, b: StripeNewBox) {
        self.release_for_head();
        assert!(self.v.len() < self.v.capacity(), "no room kept for a head's stripe");
        let Ok(()) = self.v.push(b, GFP_KERNEL) else { unreachable!("pushed within capacity") };
    }

    /// A repair's stripe, which isn't a head's: past the room kept for them.
    fn push(&mut self, b: StripeNewBox) -> Result<(), (AllocError, StripeNewBox)> {
        if let Err(e) = self.v.reserve(self.heads_held + 1, GFP_KERNEL) {
            return Err((e, b));
        }
        let Ok(()) = self.v.push(b, GFP_KERNEL) else { unreachable!("pushed within capacity") };
        Ok(())
    }

    fn take(&mut self, p: NonNull<StripeNew>) -> StripeNewBox {
        let i = self.v.iter().position(|b| core::ptr::eq(&**b, p.as_ptr())).expect("a stripe being made, in new_stripes");
        let last = self.v.len() - 1;
        self.v.swap(i, last);
        self.v.pop().expect("just found")
    }

    /// No stripes, and none a head holds.
    pub(crate) fn is_empty(&self) -> bool {
        self.v.is_empty() && self.heads_held == 0
    }
}

impl core::ops::Deref for NewStripes {
    type Target = [StripeNewBox];

    fn deref(&self) -> &[StripeNewBox] {
        &self.v
    }
}

/// Its task's pointer to it: Send, as the file comment argues.
#[derive(Clone, Copy)]
struct StripePtr(NonNull<StripeNew>);

// SAFETY: the filesystem owns what it points to, which outlives the task
unsafe impl Send for StripePtr {}

impl StripePtr {
    fn as_ptr(self) -> *mut StripeNew {
        self.0.as_ptr()
    }
}

impl StripeNew {
    /// A new stripe of @nr_data data blocks and @nr_parity parity of
    /// @blocksize sectors, in @disk_label, by @algorithm, on @devs - and its
    /// task, made - open: ec_new_stripe_alloc(). Its one io ref is its
    /// maker's.
    #[allow(clippy::too_many_arguments)]
    fn alloc(fs: &Fs, devs: c::bch_devs_mask, watermark: c::bch_watermark, disk_label: u32, algorithm: u32,
             nr_data: u32, nr_parity: u32, blocksize: u32) -> Result<StripeNewBox, BchError> {
        let b = kbox_pin_init(pin_init!(StripeNew {
            // SAFETY: its head and new_stripes, which own it, are emptied
            // going read-only
            fs:         unsafe { BackRef::new(fs) },
            start_time: local_clock(),
            idx:        AtomicU64::new(0),
            io_refs:    AtomicI32::new(1),
            in_flight   <- Gather::new(),
            seq:        AtomicU64::new(0),
            err:        AtomicI32::new(0),
            state:      AtomicU32::new(State::EC_STRIPE_NEW_open as u32),
            internal    <- new_lock!(<- Internal::new(devs, watermark)),
        }? BchError))?;

        let p = StripePtr(NonNull::from(&*b));
        {
            let mut s = b.lock();
            new_stripe_key(fs, &mut s.new_stripe, algorithm as u8, nr_data as u8, nr_parity as u8,
                           blocksize as u16, disk_label as u8);

            // SAFETY: what the task borrows outlives it - see the file comment
            s.task = Some(unsafe { spawn_unstarted(Spawn::Queue(&fs.ec.stripe_create_wq), move || stripe_task(fs, p)) }?);
        }

        // the io refs': their last put puts it
        let _ = b.in_flight.get().into_raw();
        Ok(b)
    }

    /// # Safety
    /// @p is an open bucket's ->ec: a StripeNew, live while it's borrowed.
    pub(crate) unsafe fn from_c<'a>(p: *const c::ec_stripe_new) -> &'a StripeNew {
        // SAFETY: the caller's
        unsafe { &*p.cast::<StripeNew>() }
    }

    pub(crate) fn as_c(&self) -> *mut c::ec_stripe_new {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// Its filesystem, for StripeBlock::new() and reserve_parity().
    pub(crate) fn fs_ref(&self) -> &BackRef<Fs> {
        &self.fs
    }

    /// Never held across an await
    /// - a mutex is released by the task that took it, and its task resumes
    /// on any worker - and, once it's launched, never long.
    pub(crate) fn lock(&self) -> Guard<'_, c::mutex, Internal> {
        self.internal.lock()
    }

    fn state(&self) -> State {
        State::from_bits(self.state.load(Ordering::Relaxed))
    }

    /// Its task started, unless it has been, and its slot published - @s its
    /// internal state, locked: from here on, writers may have it.
    fn launch(&self, s: &mut Internal) {
        if let Some(t) = s.task.take() {
            self.idx.store(s.new_stripe.idx(), Ordering::Relaxed);
            // detached: it removes itself from new_stripes when it's done
            drop(t.start());
        }
    }

    /// Filling: its task started, and its maker's io ref put - the maker's last
    /// touch of it. @_v, new_stripes, locked, and it's in them: so its task,
    /// which takes it out to free it, can't before we're done.
    fn fill(&self, _v: &NewStripes) {
        self.state.store(State::EC_STRIPE_NEW_filling as u32, Ordering::Relaxed);
        self.launch(&mut self.lock());
        // SAFETY: its maker's io ref, ours; new_stripes is locked
        unsafe { StripeNew::io_put(self) }
    }

    /// A writer's io ref, for the block it's claimed.
    fn io_get(&self) {
        self.io_refs.fetch_add(1, Ordering::Relaxed);
    }

    /// An io ref put: the last takes @p in flight - its seq the next, the
    /// flushes woken - and has its task go on to create it.
    ///
    /// # Safety
    /// @p is live, and holds the ref; after this, it may not be.
    unsafe fn io_put(p: *const StripeNew) {
        // SAFETY: the caller's
        let s = unsafe { &*p };
        let fs = &*s.fs;
        let v = s.io_refs.fetch_sub(1, Ordering::AcqRel);
        assert!(v > 0, "stripe {} io ref put with none held", s.idx.load(Ordering::Relaxed));
        if v != 1 {
            return;
        }

        // Every bucket is back: the data is all in. This is the filling ->
        // in_flight transition, after which the stripe completes without
        // needing anything from a writer; seq is the commit-ready marker
        // fs_ec_flush_outstanding() waits on.
        s.state.store(State::EC_STRIPE_NEW_in_flight as u32, Ordering::Relaxed);
        s.seq.store(fs.ec.stripe_new_seq.fetch_add(1, Ordering::Relaxed) as u64 + 1, Ordering::Release);
        fs.ec.new_stripes_changed.wake();
        // the last touch: the task may free it
        // SAFETY: the io refs' ref, alloc()'s
        unsafe { GatherRef::from_raw(&s.in_flight) }.put(());
    }
}

impl Internal {
    /// Open, on @devs, at @watermark: its keys zeroed, for new_stripe_key().
    fn new(devs: c::bch_devs_mask, watermark: c::bch_watermark) -> impl PinInit<Self> {
        pin_init!(Internal {
            write:               None,
            task:                None,
            devs,
            watermark,
            have_old_stripe:     false,
            allocated:           false,
            old_stripe_read:     false,
            old_stripe_read_all: false,
            blocks_gotten:       Default::default(),
            blocks_allocated:    Default::default(),
            blocks_moving:       Default::default(),
            blocks:              [const { None }; MAX_BLOCKS],
            res:                 None,
            buckets:             ArrayVec::new(),
            new_stripe           <- pin_init::init_zeroed(),
            new_buf:             None,
            old_stripe           <- pin_init::init_zeroed(),
            old_buf:             None,
            new_stripe_open:     None,
            old_stripe_open:     None,
            old_block_map:       [0; MAX_BLOCKS],
            old_blocks_nr:       0,
        })
    }

    pub(crate) fn nr_data(&self) -> u32 {
        self.new_stripe.stripe().v().nr_data()
    }

    /// Its parity's disk reservation: the stripe's sectors for each parity
    /// block, which a full stripe's data was reserved without.
    pub(crate) fn reserve_parity(&mut self, stripe_fs: &BackRef<Fs>, flags: c::bch_reservation_flags)
        -> Result<(), BchError>
    {
        let v = self.new_stripe.stripe().v();
        let (sectors, nr_parity) = (v.sectors.get().into(), v.nr_redundant.into());
        // SAFETY: into a stripe being made - an Internal is only ever a
        // StripeNew's - which the filesystem drains before it goes
        self.res.get_or_insert_with(|| DiskReservation::new(unsafe { stripe_fs.get() })).add(sectors, nr_parity, flags)
    }

    /// Its blocks' buckets into ec.stripes_new_buckets, for reconcile to
    /// leave alone while it's made: none of them, on an error.
    pub(crate) fn buckets_add(&mut self, fs: &Fs) -> Result<(), BchError> {
        let nr = self.new_stripe.stripe().v().nr_blocks.into();

        for b in self.blocks[..nr].iter().flatten() {
            let bucket = bucket_to_u64(pos(b.dev().into(), b.bucket()));

            match fs.ec.stripes_new_buckets.insert_new(bucket) {
                Ok(Ok(())) => self.buckets.push(bucket),
                Ok(Err(_)) => panic!("bucket {}:{} in two stripes being made", b.dev(), b.bucket()),
                Err(AllocError) => {
                    self.buckets_del(fs);
                    return fs.throw(bch_errcode::BCH_ERR_ENOMEM_stripe_new_bucket);
                }
            }
        }
        Ok(())
    }

    /// buckets_add()'s buckets out.
    fn buckets_del(&mut self, fs: &Fs) {
        for b in self.buckets.drain(..) {
            fs.ec.stripes_new_buckets.remove(&b);
        }
    }
}

#[pinned_drop]
impl PinnedDrop for StripeNew {
    /// Whatever it still holds given back: what a rebuild that failed has
    /// got, and every stripe's slot claims and hashed buckets -
    /// bch2_ec_stripe_new_free() and stripe_repair_rebuild_abort() - and a
    /// repair's writes, done. Its buffers go with it.
    fn drop(self: Pin<&mut Self>) {
        let fs = &*self.fs;
        let mut s = self.lock();

        s.blocks = Default::default();
        s.res = None;
        s.buckets_del(fs);

        for claim in [s.new_stripe_open.take(), s.old_stripe_open.take()].into_iter().flatten() {
            claim.put(fs);
        }
        drop(s.write.take());
    }
}

/// @b, never launched, freed: a repair's rebuild that failed, or a head's
/// stripe at shutdown. Only its own io ref is held - no writer has a block -
/// and its task, never started, goes with it.
fn discard(b: StripeNewBox) {
    assert!(b.lock().task.is_some(), "discarding stripe {} with its task started", b.idx.load(Ordering::Relaxed));
    let v = b.io_refs.swap(0, Ordering::Relaxed);
    assert_eq!(v, 1, "discarding stripe {} with writers' io refs", b.idx.load(Ordering::Relaxed));
    // SAFETY: the io refs' ref on its Gather, which its drop waits for
    unsafe { GatherRef::from_raw(&b.in_flight) }.put(());
    drop(b);
}

fn new_stripes_take(fs: &Fs, p: NonNull<StripeNew>) -> StripeNewBox {
    fs.ec.new_stripes.lock().take(p)
}

// ── The head's: staging, and claiming blocks ──────────────────────────────

/// A new stripe for @h: as wide as @h's devices allow - one block per
/// failure domain is a hard requirement, so the width is capped by domains
/// as well as devices; if a domain goes away the next stripe is narrower
/// rather than doubling up.
fn alloc_for_head(fs: &Fs, h: &c::ec_stripe_head, g: &mut HeadGuard<'_>, max_data_blocks: u32)
    -> Result<StripeNewBox, BchError>
{
    let active = g.nr_active_devs.min(c::BCH_BKEY_PTRS_MAX);
    let nr_data = (active - h.redundancy).min(if max_data_blocks != 0 { max_data_blocks } else { !0 });

    // insufficient_devs was checked - at least redundancy + 2 domains:
    let nr_domains = fs.target_nr_domains(&g.devs);
    let nr_data = nr_data.min(nr_domains - h.redundancy);

    let failed = || {
        bch_err!(fs, "failed to allocate new stripe");
        fs.err(bch_errcode::BCH_ERR_ENOMEM_ec_new_stripe_alloc)
    };
    // its room in new_stripes, for when it's handed on
    fs.ec.new_stripes.lock().reserve_for_head().map_err(|_| failed())?;
    let s = StripeNew::alloc(fs, g.devs, h.watermark, h.disk_label, h.algo, nr_data, h.redundancy, g.blocksize)
        .map_err(|_| {
            fs.ec.new_stripes.lock().release_for_head();
            failed()
        })?;
    g.nr_created += 1;
    Ok(s)
}

/// Hand @b on to its task, as it is: it's no longer its head's, and goes in
/// new_stripes, its own io ref put. The caller's lock - the head's - is held.
pub(crate) fn set_pending(fs: &Fs, b: StripeNewBox) {
    {
        let i = b.lock();
        assert!(i.allocated || b.err.load(Ordering::Relaxed) != 0,
                "stripe {} handed on, neither allocated nor cancelled", i.new_stripe.idx());
    }

    let mut v = fs.ec.new_stripes.lock();
    v.push_from_head(b);
    v.last().expect("just pushed").fill(&v);
}

/// A block of @s, @i its internal state, for this write: the first one on a
/// device we may allocate from that nobody else has taken - its io ref, the
/// writer's. NULL means this stripe has nothing for us: its free blocks are
/// all on devices we're excluded from.
fn claim_bucket(req: &c::alloc_request, s: &StripeNew, i: &mut Internal) -> *mut c::open_bucket {
    let nr_data = i.nr_data() as usize;
    for &dev in &req.devs_sorted.data[..req.devs_sorted.nr as usize] {
        for ec_idx in 0..nr_data {
            let Some(b) = &i.blocks[ec_idx] else { continue };
            if b.dev() == u32::from(dev) && !i.blocks_allocated.test_and_set(ec_idx) {
                let Some(StripeBlock::Pinned(b)) = i.blocks[ec_idx].take() else {
                    panic!("stripe {} block {ec_idx} claimed twice", s.idx.load(Ordering::Relaxed))
                };
                let (claimed, ob) = b.claim(s.as_c(), ec_idx as u8);
                i.blocks[ec_idx] = Some(StripeBlock::Claimed(claimed));
                s.io_get();
                return ob;
            }
        }
    }
    core::ptr::null_mut()
}

/// @h's stripe staged - allocated, its buffers, its task started - and a
/// block of it claimed for this write, if @want_ob: NULL if it has none for
/// us.
fn head_stage(t: &TransAttempt<'_, '_>, req: &mut c::alloc_request, h: &c::ec_stripe_head, g: &mut HeadGuard<'_>,
              want_ob: bool) -> Result<*mut c::open_bucket, BchError> {
    let fs = t.fs();
    if g.s.is_none() {
        let s = alloc_for_head(fs, h, g, req.ec_max_data_blocks.into())?;
        g.s = Some(s);
    }

    let s = g.s.as_deref().expect("the head's stripe, just made");
    let mut i = s.lock();
    if !i.allocated {
        stripe_alloc_or_reuse(t, req, dev_stripe_state_get(t, h.disk_label)?, &mut i, s.fs_ref())?;

        fs.event_inc_trace(events::stripe_alloc, |out| {
            prt!(out, "\nnew: {}", i.new_stripe.stripe().to_text(fs));
            if i.have_old_stripe {
                prt!(out, "\nold: {}", i.old_stripe.stripe().to_text(fs));
            }
        });
    }

    if i.new_buf.is_none() {
        let wait = NonNull::new(req.cl).map_or(MemWait::Never, MemWait::Closure);
        i.new_buf = Some(StripeBuf::new_whole(fs, i.new_stripe.stripe(), wait)?);
    }

    if !i.old_stripe_read && i.have_old_stripe {
        i.old_buf = Some(StripeBuf::new_whole(fs, i.old_stripe.stripe(), MemWait::Never)?);
        i.old_stripe_read = true;
    }

    s.launch(&mut i);

    // Has to come after the stripe's own allocation: that runs the bucket
    // allocator against the stripe's device mask, clobbering devs_sorted.
    let ob = if want_ob {
        // SAFETY: the write's write point, the caller's
        fs.dev_alloc_list(unsafe { &mut (*req.wp).alloc.stripe }, req);
        claim_bucket(req, s, &mut i)
    } else {
        core::ptr::null_mut()
    };

    t.verify_not_restarted();
    Ok(ob)
}

/// The stripe head for @req's erasure coding, locked, with a block of its
/// stripe claimed for this write in @ob - if @ob isn't NULL, and it had one
/// for us: or the error, an ec_alloc_failed subtype naming the reason.
/// bch2_ec_stripe_head_get().
fn head_get<'f>(t: &TransAttempt<'_, 'f>, req: &mut c::alloc_request, algo: u32, want_ob: bool)
    -> Result<(&'f c::ec_stripe_head, HeadGuard<'f>, *mut c::open_bucket), BchError>
{
    let fs = t.fs();
    let redundancy = u32::from(req.ec_replicas).wrapping_sub(1);
    let disk_label = match Target::decode(req.target) {
        Target::Group(g) if g > u8::MAX.into() => return fs.throw(bch_errcode::BCH_ERR_ec_alloc_failed_disk_label_too_big),
        Target::Group(g) => g + 1, /* 0 == no label */
        _ => 0,
    };

    let (h, mut g) = stripe_head_get(t, disk_label, algo, redundancy, req.watermark)?;
    match head_stage(t, req, h, &mut g, want_ob) {
        Ok(ob) => Ok((h, g, ob)),
        Err(e) => {
            stripe_head_put(fs, g);
            // Not fatal to the write - the allocator falls back to replication:
            Err(if e.matches(bch_errcode::BCH_ERR_stripe_insufficient_devices) {
                fs.err(bch_errcode::BCH_ERR_ec_alloc_failed_stripe_alloc)
            } else {
                e
            })
        }
    }
}

/// head_get(), for C: the head locked, until bch2_ec_stripe_head_put().
///
/// # Safety
/// @trans in an attempt; @req the caller's, its write point too; @ob NULL,
/// or where to put the block claimed.
#[c_export]
pub unsafe fn bch2_ec_stripe_head_get<'t>(trans: &'t BtreeTrans<'t>, req: &mut c::alloc_request, algo: c_uint,
                                          ob: *mut *mut c::open_bucket) -> *mut c::ec_stripe_head {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    if let Some(ob) = unsafe { ob.as_mut() } {
        *ob = core::ptr::null_mut();
    }
    match head_get(&t, req, algo, !ob.is_null()) {
        Ok((h, g, got)) => {
            g.leave_locked();
            if let Some(ob) = unsafe { ob.as_mut() } {
                *ob = got;
            }
            core::ptr::from_ref(h).cast_mut()
        }
        Err(e) => err_ptr(e),
    }
}

// ── Its writers' ───────────────────────────────────────────────────────────

/// A writer's io ref on @s put: its open bucket's last pin put.
///
/// # Safety
/// @s is the open bucket's ->ec, whose io ref the caller drops.
#[c_export]
pub unsafe fn bch2_ec_stripe_new_put_io(s: *mut c::ec_stripe_new) {
    // SAFETY: the caller's
    unsafe { StripeNew::io_put(s.cast()) }
}

/// The stripe @ob is a block of is cancelled, with @err: the writer's IO
/// failed.
///
/// # Safety
/// @ob is an open bucket with an ->ec, the caller's pin holding it open.
#[c_export]
pub unsafe fn bch2_ec_bucket_cancel(_c: &Opaque<c::bch_fs>, ob: &c::open_bucket, err: c_int) {
    // SAFETY: the caller's
    let ob = unsafe { OpenBucketRef::from_c(ob) };
    // SAFETY: its io ref keeps it
    unsafe { StripeNew::from_c(ob.ec()) }.err.store(err, Ordering::Relaxed);
}

/// Where @wp's write goes in its stripe's buffer, if one of its open buckets
/// is a block of a stripe being made: NULL if none is.
///
/// # Safety
/// @wp is a write point, locked.
#[c_export]
pub unsafe fn bch2_writepoint_ec_buf(c: &Opaque<c::bch_fs>, wp: &c::write_point) -> *mut c_void {
    let fs = Fs::from_c(c);
    let ptrs = &wp.alloc.ptrs;
    let Some(ob) = ptrs.v[..usize::from(ptrs.nr)].iter()
        // SAFETY: the write point's pins, which its lock - the caller's - keeps
        .map(|&i| unsafe { OpenBucketRef::from_idx(fs, i.expect("a bucket of the list")) })
        .find(|ob| !ob.ec().is_null()) else { return core::ptr::null_mut() };

    let offset = {
        let rcu = read_lock();
        let ca = fs.dev_rcu(ob.dev(), &rcu).expect("an open bucket's device");
        u32::from(ca.mi.bucket_size) - ob.sectors_free()
    };

    // SAFETY: the write point's pin, and its io ref, keep its stripe
    let mut s = unsafe { StripeNew::from_c(ob.ec()) }.lock();
    let buf = s.new_buf.as_mut().expect("a stripe with blocks claimed has its buffer");
    // its writer's to write, at its write point's free space
    buf.block_mut(usize::from(ob.ec_idx()))[(offset as usize) << 9..].as_mut_ptr().cast()
}

/// Whether @s has a block on device @dev: for the allocator stopping it,
/// which drops its writers' open buckets on a stripe that does.
///
/// # Safety
/// @s is an open bucket's ->ec.
#[c_export]
pub unsafe fn bch2_ec_stripe_new_has_dev(_c: &Opaque<c::bch_fs>, s: *const c::ec_stripe_new, dev: c_uint) -> bool {
    // SAFETY: the caller's
    let s = unsafe { StripeNew::from_c(s) }.lock();
    let nr_blocks = s.new_stripe.stripe().v().nr_blocks as usize;
    s.blocks[..nr_blocks].iter().flatten().any(|b| b.dev() == dev)
}

/// @s's slot, for open_bucket_to_text() - under spinlocks, so not by its
/// lock: launch() published it.
///
/// # Safety
/// @s is an open bucket's ->ec.
#[c_export]
pub unsafe fn bch2_ec_stripe_new_idx(s: *const c::ec_stripe_new) -> u64 {
    // SAFETY: the caller's
    unsafe { StripeNew::from_c(s) }.idx.load(Ordering::Relaxed)
}

// ── Its task ─────────────────────────────────────────────────────────────

/// The old stripe's read, failed: its error, and the carried blocks it cost
/// us - the data the failure lost.
struct OldStripeLost {
    err:    BchError,
    blocks: u32,
}

async fn stripe_task(fs: &Fs, p: StripePtr) {
    let at = Resume::Queue(&fs.ec.stripe_create_wq);
    stack_pin_init!(let w = Working::new());
    let w = w.get_mut();

    let fold = {
        // SAFETY: the filesystem owns it until teardown(), below
        let s = unsafe { &*p.as_ptr() };

        // set before its start, by its head or repair
        let (read, read_all) = {
            let i = s.lock();
            (i.old_stripe_read, i.old_stripe_read_all)
        };
        if read && !read_all { old_stripe_fold(fs, s, w, at).await } else { Ok(()) }
    };

    // SAFETY: the task is the one waiter on it, its owner
    unsafe { Pin::new_unchecked(&mut *(&raw mut (*p.as_ptr()).in_flight)) }.wait().resume_on(at).await;

    // As C's, whatever state writes are in: a stripe goes in flight while
    // they're stopped too - bch2_open_buckets_stop(), in
    // __bch2_fs_read_only(), puts the last buckets of full stripes after
    // c->writes is drained.
    let _w = fs.writes.get();

    // SAFETY: as above
    let s = unsafe { &*p.as_ptr() };
    let ret = create(fs, s, w, fold, at).await;
    create_done(fs, s, ret);
    // SAFETY: nothing after this touches it
    unsafe { teardown(fs, p) };
}

/// Only the blocks we carry forward have to be good - everything else in the
/// old stripe is discarded at the fold, parity included, and regenerated at
/// create.
fn old_stripe_required(s: &Internal) -> u32 {
    s.old_block_map[..usize::from(s.old_blocks_nr)].iter().fold(0, |m, &b| m | 1 << b)
}

/// The old stripe's blocks we're carrying forward, in @old, checked out: take
/// them - their buffers swapped into the new stripe's, whose carried slots
/// are no writer's, as they're in blocks_allocated.
fn old_stripe_carry(s: &mut Internal, old: &mut StripeBuf) {
    let Internal { new_buf, old_block_map, old_blocks_nr, .. } = s;
    let new = new_buf.as_mut().expect("a new stripe's buffer");
    assert_eq!((new.offset, new.size), (old.offset, old.size),
               "carrying blocks between stripe buffers of other sizes");

    for (i, &j) in old_block_map[..usize::from(*old_blocks_nr)].iter().enumerate() {
        core::mem::swap(&mut new.data[i], &mut old.data[usize::from(j)]);
    }
}

/// Fold the blocks we're carrying forward into the new stripe as soon as the
/// old stripe's read lands, instead of at create: ec_old_stripe_read() and
/// ec_old_stripe_fold(). The carried blocks are set in blocks_allocated, so
/// the sector allocator will never hand one to a writer, and the data we
/// swap in can't be overwritten while the rest of the stripe fills.
///
/// Only the carried blocks are read. If one of them is bad, the rest are
/// read so it can be reconstructed: then the old stripe's validated,
/// reconstructing what's bad if it can be, and its blocks carried - or the
/// ones we wanted that are lost said, for create to record damage against.
async fn old_stripe_fold(fs: &Fs, s: &StripeNew, w: &mut Working, at: Resume<'_>) -> Result<(), OldStripeLost> {
    // The old stripe's key and buffer, to read with across the awaits; the
    // buffer back after
    let required = {
        let mut i = s.lock();
        w.old_stripe.copy_from(&i.old_stripe);
        core::mem::swap(&mut w.old_buf, &mut i.old_buf);
        old_stripe_required(&i)
    };

    let buf = w.old_buf.as_mut().expect("the old stripe's buffer");
    let ret = old_stripe_read(fs, &w.old_stripe, buf, required, at).await;

    let mut i = s.lock();
    if ret.is_ok() {
        old_stripe_carry(&mut i, buf);
    }
    core::mem::swap(&mut w.old_buf, &mut i.old_buf);
    ret
}

/// The old stripe's @required blocks, into @buf - and the rest if one's bad,
/// to reconstruct it: whether they're good.
async fn old_stripe_read(fs: &Fs, old: &StripeKey, buf: &mut StripeBuf, required: u32, at: Resume<'_>)
    -> Result<(), OldStripeLost>
{
    let all = ((1u64 << old.stripe().v().nr_blocks) - 1) as u32;

    {
        stack_pin_init!(let io = BlockIo::read(fs, buf));
        for i in blocks_in(required) {
            io.as_mut().submit(old.stripe(), i);
        }
        io.as_mut().wait(at).await;
    }

    if !blocks_good(old.stripe(), buf, required) {
        {
            stack_pin_init!(let io = BlockIo::read(fs, buf));
            for i in blocks_in(all & !required) {
                io.as_mut().submit(old.stripe(), i);
            }
            io.as_mut().wait(at).await;
        }

        if let Err(err) = validate_msg(fs, old.stripe(), buf, true, required, c"ec_old_stripe_read_done") {
            // Reconstruct either couldn't run - too many failures, so
            // everything that failed to read is lost - or ran and left blocks
            // that still don't check out:
            let e = if err == BchError::from(bch_errcode::BCH_ERR_stripe_reconstruct_insufficient_blocks) { PRE } else { POST };
            return Err(OldStripeLost { err, blocks: buf.failed_mask(e) & required });
        }
    }
    Ok(())
}

/// Whether a device of @v's is being removed.
fn stripe_has_removing_dev(fs: &Fs, v: &c::Stripe<Validated>) -> bool {
    let rcu = read_lock();
    v.block_ptrs().iter().any(|p| fs.dev_rcu(p.dev().into(), &rcu).is_some_and(|ca| ca.removing()))
}

/// The new stripe's key, to commit: its reconcile state set, and no key at
/// its slot already.
fn stripe_key_update(t: &TransAttempt<'_, '_>, k: BkeySC<'_, c::Stripe<Validated>>) -> Result<(), BchError> {
    let fs = t.fs();
    let mut new = t.bkey_make_mut_noupdate(k)?;

    let opts = inode_opts::get_fs(fs, false);
    new.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_foreground, 0)?;

    let mut iter = BtreeIter::new(t, c::btree_id::stripes, k.k.p, BtreeIterFlags::INTENT);
    let old = iter.peek_slot(t)?.expect("a slot always has a key");
    if old.k.type_ != 0 {
        fs_inconsistent!(fs, "error creating stripe: got existing key\n{}", old.to_text(fs));
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ec_stripe_create_existing_key);
    }
    t.update(&iter, &new, UpdateTriggerFlags::empty())
}

/// What the task works on: its stripe's keys and buffers, and what create
/// needs of its internal state - out from under its lock, for the IO, which
/// is awaited, and back after. The task's own, made in place: see the file
/// comment.
struct Working {
    new_stripe:      StripeKey,
    new_buf:         Option<StripeBuf>,
    old_stripe:      StripeKey,
    old_buf:         Option<StripeBuf>,
    res:             Option<DiskReservation<'static>>,
    blocks_moving:   c::StripeBlockMask,
    old_block_map:   [u8; MAX_BLOCKS],
    old_blocks_nr:   u8,
    have_old_stripe: bool,
    allocated:       bool,
    watermark:       c::bch_watermark,
}

impl Working {
    /// Nothing yet: the fold and take() fill it.
    fn new() -> impl Init<Self> {
        init!(Working {
            new_stripe      <- init_zeroed(),
            new_buf:        None,
            old_stripe      <- init_zeroed(),
            old_buf:        None,
            res:            None,
            blocks_moving   <- init_zeroed(),
            old_block_map   <- init_zeroed(),
            old_blocks_nr:  0,
            have_old_stripe: false,
            allocated:      false,
            watermark       <- init_zeroed(),
        })
    }

    /// @i's, out from under its lock: its keys copied, and what it says of
    /// them; its buffers and reservation swapped in.
    fn take(&mut self, i: &mut Internal) {
        self.new_stripe.copy_from(&i.new_stripe);
        self.old_stripe.copy_from(&i.old_stripe);
        self.blocks_moving   = i.blocks_moving;
        self.old_block_map   = i.old_block_map;
        self.old_blocks_nr   = i.old_blocks_nr;
        self.have_old_stripe = i.have_old_stripe;
        self.allocated       = i.allocated;
        self.watermark       = i.watermark;
        self.swap(i);
    }

    /// Back to @i: the new key, as create edited it, and the buffers and
    /// what's left of the reservation.
    fn put_back(&mut self, i: &mut Internal) {
        i.new_stripe.copy_from(&self.new_stripe);
        self.swap(i);
    }

    fn swap(&mut self, i: &mut Internal) {
        core::mem::swap(&mut self.new_buf, &mut i.new_buf);
        core::mem::swap(&mut self.old_buf, &mut i.old_buf);
        core::mem::swap(&mut self.res, &mut i.res);
    }
}

/// Data buckets of the new stripe all written: create the stripe, with the
/// device io refs it'll make references to - guarding against racing with
/// device removal.
async fn create(fs: &Fs, s: &StripeNew, w: &mut Working, fold: Result<(), OldStripeLost>, at: Resume<'_>)
    -> Result<(), BchError>
{
    w.take(&mut s.lock());
    let ret = create_iorefs(fs, s, w, fold, at).await;
    w.put_back(&mut s.lock());
    ret
}

async fn create_iorefs(fs: &Fs, s: &StripeNew, w: &mut Working, fold: Result<(), OldStripeLost>,
                       at: Resume<'_>) -> Result<(), BchError> {
    // Kept by device, not by c->devs[idx]: dev_remove may clear that slot
    // while our io ref still pins the device.
    let mut cas: ArrayVec<DevIoRef<'_, c::bch_dev_write_ref>, MAX_BLOCKS> = ArrayVec::new();
    for ptr in w.new_stripe.stripe().v().block_ptrs() {
        let Some(ca) = fs.dev_get_ioref::<c::bch_dev_write_ref>(ptr.dev().into()) else {
            return fs.throw(bch_errcode::BCH_ERR_stripe_create_device_offline);
        };
        cas.push(ca);
    }

    create_with_iorefs(fs, s, w, fold, at).await
}

async fn create_with_iorefs(fs: &Fs, s: &StripeNew, w: &mut Working, fold: Result<(), OldStripeLost>,
                            at: Resume<'_>) -> Result<(), BchError>
{
    let e = s.err.load(Ordering::Relaxed);
    if e != 0 {
        let e = BchError::from_raw(-e);
        if !e.matches(c::EROFS) {
            bch_err!(fs, "error creating stripe: error writing data buckets");
        }
        return Err(e);
    }

    // bch2_dev_remove() sets ->removing before its data drop, then flushes
    // outstanding creates, so checking before we commit is enough. Only the
    // new key's pointers matter: blocks being moved off the device get fresh
    // buckets, and reading it during the drop is how their data moves. What
    // must not commit is a pointer copied while the device was healthy - a
    // reused stripe's - after the removal walk has passed it.
    if stripe_has_removing_dev(fs, w.new_stripe.stripe().v()) {
        return fs.throw(bch_errcode::BCH_ERR_stripe_create_device_removing);
    }

    let (nr_data, nr_blocks) = {
        let v = w.new_stripe.stripe().v();
        (v.nr_data() as usize, usize::from(v.nr_blocks))
    };
    let Working { new_stripe, new_buf, old_stripe, old_buf, blocks_moving, old_block_map, old_blocks_nr,
                  have_old_stripe, allocated, watermark, res } = w;
    let buf = new_buf.as_mut().expect("a new stripe's buffer");

    stack_pin_init!(let io = BlockIo::write(fs, buf));

    // the rest of each data bucket zeroed: zero_out_rest_of_ec_bucket()
    for i in usize::from(*old_blocks_nr)..nr_data {
        // its writer done with it
        let Some((dev, sectors_free)) = s.lock().blocks[i].as_ref().map(|b| (b.dev(), b.sectors_free())) else {
            continue
        };
        if sectors_free == 0 {
            continue;
        }
        let offset = {
            let rcu = read_lock();
            let ca = fs.dev_rcu(dev, &rcu).expect("an open bucket's device");
            u32::from(ca.mi.bucket_size) - sectors_free
        };
        let start = (offset as usize) << 9;
        io.as_mut().buf().block_mut(i)[start..start + ((sectors_free as usize) << 9)].fill(0);
        io.as_mut().submit_range(new_stripe.stripe(), i, offset, sectors_free);
    }

    // The fold was awaited before we were in flight: its error is settled.
    // The reuse is abandoned, so the old stripe stays as it is and nothing
    // else discovers the loss until someone reads one of those files:
    if let Err(lost) = &fold {
        io.as_mut().wait(at).await;

        // Too few blocks to reconstruct: the lost blocks' buffers still hold
        // what we read, and an extent that checks out against it isn't lost
        // - a block fails on one bad granule. After a reconstruct they hold
        // its output instead, which says nothing about what's on disk.
        let as_read = lost.err == BchError::from(bch_errcode::BCH_ERR_stripe_reconstruct_insufficient_blocks);
        let trans = btree_trans!(fs);
        record_lost_blocks(&trans, old_stripe.stripe(), lost.blocks,
                           c::bch_sb_error_id::BCH_FSCK_ERR_stripe_reconstruct_failed, false,
                           old_buf.as_ref().filter(|_| as_read));
        return Err(lost.err);
    }

    assert!(*allocated, "creating stripe {}, not allocated", new_stripe.idx());

    generate_ec(new_stripe.stripe(), io.as_mut().buf());
    generate_checksums(new_stripe, io.as_mut().buf());

    // write out data blocks that moved
    for i in 0..usize::from(*old_blocks_nr) {
        if blocks_moving.test(i) {
            io.as_mut().submit(new_stripe.stripe(), i);
        }
    }

    // write p/q:
    for i in nr_data..nr_blocks {
        io.as_mut().submit(new_stripe.stripe(), i);
    }
    io.as_mut().wait(at).await;

    if io.as_mut().buf().failed_mask(PRE) != 0 {
        bch_err!(fs, "error creating stripe: error writing redundancy buckets");
        return fs.throw(bch_errcode::BCH_ERR_ec_block_write);
    }

    let new = new_stripe.stripe();
    let old_idx = if *have_old_stripe { old_stripe.idx() } else { 0 };
    let new_idx = new_stripe.idx();

    let mut op = BkeyI::<c::LoggedOpStripeUpdate>::new();
    op.v.old_idx       = old_idx.into();
    op.v.new_idx       = new_idx.into();
    op.v.old_blocks_nr = *old_blocks_nr;
    op.v.old_block_map = *old_block_map;

    let watermark: Watermark = create_watermark(*watermark).into();
    let opts = CommitOpts::new().watermark(watermark).flags(CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC);

    let res = match res.take() {
        Some(res) => res,
        None      => DiskReservation::new(fs),
    };
    trans_commit_do(fs, Some(&res), opts, |t| {
        stripe_key_update(t, new)?;
        logged_ops::start(t, op.as_mut())
    })?;

    let trans = btree_trans!(fs);
    let map = &old_block_map[..usize::from(*old_blocks_nr)];
    // no old stripe, if it isn't a reuse: nothing's carried from it
    let old = if map.is_empty() { new } else { old_stripe.stripe() };
    let ret = stripe_update_extents(&trans, old, new, map, watermark);
    logged_ops::finish(&trans, op.as_ref(), ret, CommitOpts::new().watermark(watermark))?;

    // The trigger and the delete worker skip open stripes, and both of ours
    // may have emptied while we held them:
    commit_do(&trans, None, opts, |t| {
        if old_idx != 0 && old_idx != new_idx {
            stripe_delete(t, old_idx, true)?;
        }
        stripe_delete(t, new_idx, true)
    })
}

/// What creating @s came to, counted and traced.
fn create_done(fs: &Fs, s: &StripeNew, ret: Result<(), BchError>) {
    let i = s.lock();
    let new = i.new_stripe.stripe();
    match ret {
        Err(e) => {
            if s.err.load(Ordering::Relaxed) == 0 {
                s.err.store(ret_to_c(Err(e)), Ordering::Relaxed);
            }
            fs.event_inc_trace(events::stripe_create_fail, |out| {
                prt!(out, "error {e}\n{}", new.to_text(fs));
            });
        }
        Ok(()) if i.have_old_stripe => fs.event_inc_trace(events::stripe_reuse, |out| {
            let ov = i.old_stripe.stripe();
            prt!(out, "Reused {}/{} data blocks\n", i.old_blocks_nr, ov.v().nr_data());
            prt!(out, "\nOld: {}", ov.to_text(fs));
            prt!(out, "\nNew: {}", new.to_text(fs));
        }),
        Ok(()) => fs.event_inc_trace(events::stripe_create, |out| {
            prt!(out, "\n{}", new.to_text(fs));
        }),
    }
}

/// Its buckets given back, and it out of new_stripes and freed - a repair's
/// writes done with it - then the flushes woken.
///
/// # Safety
/// The task's, in flight; nothing touches it after.
unsafe fn teardown(fs: &Fs, p: StripePtr) {
    {
        // SAFETY: the caller's
        let mut s = unsafe { &*p.as_ptr() }.lock();
        // its writers done: the blocks they claimed closed, the rest put
        s.blocks = Default::default();
    }

    drop(new_stripes_take(fs, p.0));
    fs.ec.new_stripes_changed.wake();
}

// ── Waiting for them ─────────────────────────────────────────────────────

impl Fs {
    /// Sleep until @done, checked with new_stripes locked - so not in the
    /// sleep's state, as the lock may sleep.
    fn new_stripes_wait(&self, mut done: impl FnMut(&[StripeNewBox]) -> bool) {
        loop {
            let seen = self.ec.new_stripes_changed.snapshot();
            if done(&self.ec.new_stripes.lock()) {
                return;
            }
            let Ok(()) = self.ec.new_stripes_changed.wait(seen, Sleep::UNINTERRUPTIBLE);
        }
    }
}

impl Fs {
    /// Wait for every stripe being made to be done with.
    pub fn ec_flush(&self) {
        self.new_stripes_wait(|v| v.is_empty())
    }

    /// Wait for every stripe that had gone in flight - its io refs drained,
    /// its seq assigned - when we were called to finish committing. Those
    /// still filling (seq 0) are skipped: they either complete after us, a
    /// later seq, or get cancelled via their io refs, which we shouldn't
    /// block on from under state_lock - the caller's own teardown
    /// (bch2_ec_stop_dev(), say) cancels them before this.
    pub fn ec_flush_outstanding(&self) {
        let wait_seq = self.ec.stripe_new_seq.load(Ordering::Relaxed) as u64;
        self.new_stripes_wait(|v| !v.iter().any(|s| {
            let seq = s.seq.load(Ordering::Acquire);
            seq != 0 && seq <= wait_seq
        }))
    }
}

#[c_export]
pub fn bch2_fs_ec_flush(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).ec_flush()
}

#[c_export]
pub fn bch2_fs_ec_flush_outstanding(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).ec_flush_outstanding()
}

/// At shutdown, with no stripe being made but the heads': @b, a head's
/// stripe, taken from it, freed.
pub(crate) fn head_stripe_free(fs: &Fs, b: StripeNewBox) {
    fs.ec.new_stripes.lock().release_for_head();
    discard(b);
}

// ── To text ──────────────────────────────────────────────────────────────

const STATE_NAMES: [&str; State::EC_STRIPE_NEW_STATE_NR as usize] = {
    macro_rules! names {
        ($(($n:ident)),* $(,)?) => { [$(stringify!($n)),*] };
    }
    c::EC_STRIPE_NEW_STATES!(names)
};

/// @s: its key, its blocks, and how long it's been being made:
/// bch2_new_stripe_to_text(). Blocks are listed when handed out, not when
/// full, so per claimed block: is its bucket still checked out, and how much
/// room is left?
#[cold]
pub(crate) fn new_stripe_to_text(out: &mut Printbuf, fs: &Fs, s: &StripeNew) {
    let i = s.lock();
    let v = i.new_stripe.stripe().v();
    prt!(out, "\tidx {} blocks {}+{} allocated {} ref {} {} {} obs",
         i.new_stripe.idx(), v.nr_data(), v.nr_redundant,
         (0..v.nr_data() as usize).filter(|&b| i.blocks_allocated.test(b)).count(),
         s.io_refs.load(Ordering::Relaxed), i.watermark.name(), STATE_NAMES[s.state() as usize]);

    for b in i.blocks_gotten.iter().filter(|&b| b < usize::from(v.nr_blocks)) {
        prt!(out, " {}", i.blocks[b].as_ref().map_or(0, |ob| ob.idx().get()));

        let Some(StripeBlock::Claimed(ob)) = &i.blocks[b] else { continue };
        let rcu = read_lock();
        if let Some(ca) = fs.dev_rcu(ob.dev(), &rcu) {
            let bucket_size = u32::from(ca.mi.bucket_size);
            prt!(out, "(pin {} {}/{})", ob.pin(), bucket_size - ob.sectors_free(), bucket_size);
        }
    }
    out.newline();
    prt!(out, "{}", i.new_stripe.stripe().to_text(fs));
    out.newline();

    prt!(out, "age:\t");
    out.time_units(local_clock() - s.start_time);
    out.newline();
}

impl Fs {
    /// Stripe buffer memory, the stripe heads, and the stripes being made.
    #[cold]
    pub fn new_stripes_to_text(&self, out: &mut Printbuf) {
        let (in_use, limit) = self.stripe_buf_mem();
        prt!(out, "stripe buf memory: ");
        out.human_readable_u64(in_use as u64);
        prt!(out, "/");
        out.human_readable_u64(limit as u64);
        out.newline();

        crate::data::ec::create::stripe_heads_to_text(out, self);

        prt!(out, "in flight:\n");
        for s in self.ec.new_stripes.lock().iter().filter(|s| s.state() != State::EC_STRIPE_NEW_open) {
            new_stripe_to_text(out, self, s);
        }
    }
}

#[cold]
#[c_export]
pub fn bch2_logged_op_stripe_update_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let k = k.arm::<c::LoggedOpStripeUpdate>().expect("a stripe update op");
    let op = k.v();

    prt!(out, "old_idx={}", op.old_idx.get());
    prt!(out, " new_idx={}", op.new_idx.get());
    prt!(out, " old_blocks_nr={}", op.old_blocks_nr);

    let nr = usize::from(op.old_blocks_nr).min(op.old_block_map.len());
    prt!(out, " old_block_map=");
    for (i, b) in op.old_block_map[..nr].iter().enumerate() {
        prt!(out, "{}{}", if i != 0 { "," } else { "" }, b);
    }
}

// ── Repair: reading the stripe, and rebuilding it ────────────────────────
//
// Deciding which, and the rest of repair, is create.rs's.

/// Read @s - which the caller has open - as the fold would: the blocks in
/// @required, then the rest if one of those is bad (the bool). Waits for
/// buffer memory with no btree locks held.
pub(crate) fn repair_read(trans: &BtreeTrans<'_>, s: BkeySC<'_, c::Stripe<Validated>>, required: u32)
    -> Result<(StripeBuf, bool), BchError>
{
    let fs = trans.fs();
    let v = s.v();
    let bytes = (usize::from(v.sectors.get()) << 9) * usize::from(v.nr_blocks);

    let mut buf = loop {
        match StripeBuf::new_whole(fs, s, MemWait::Blocked) {
            Err(e) if e.matches(bch_errcode::BCH_ERR_operation_blocked) => {
                trans.unlock_long();
                fs.stripe_buf_mem_wait(bytes);
            }
            ret => break ret?,
        }
    };

    trans.unlock_long();

    read_sync(fs, s, &mut buf, required);
    let read_all = !blocks_good(s, &buf, required);
    if read_all {
        read_sync(fs, s, &mut buf, !required);
    }
    Ok((buf, read_all))
}

/// Rebuild @s as a new stripe of the data blocks in @carry and @move_ - see
/// init_new_stripe_from_old() - taking over the caller's claim @h and its
/// read of the stripe, @read; its task does the writes.
pub(crate) fn repair_rebuild(ctxt: &Moving<'_>, s: BkeySC<'_, c::Stripe<Validated>>, h: OpenStripe<'_>,
                             mut read: StripeBuf, devs: c::bch_devs_mask, carry: u32, move_: u32) -> Result<(), BchError> {
    let trans = ctxt.trans();
    let fs = trans.fs();
    let v = s.v();
    let (nr_data, nr_new_data) = (v.nr_data(), (carry | move_).count_ones());

    let b = StripeNew::alloc(fs, devs, c::bch_watermark::BCH_WATERMARK_normal, v.disk_label.into(),
                             v.bits.algorithm().into(), nr_new_data, v.nr_redundant.into(), v.sectors.get().into())
        .map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_ec_new_stripe_alloc))?;
    let p = NonNull::from(&*b);
    // no head's: in new_stripes from the start
    if let Err((_, b)) = fs.ec.new_stripes.lock().push(b) {
        discard(b);
        return fs.throw(bch_errcode::BCH_ERR_ENOMEM_ec_new_stripe_alloc);
    }
    // SAFETY: in new_stripes, which keeps it till it's discarded or its task
    // frees it
    let ns = unsafe { p.as_ref() };
    // ours, unlaunched: no one else's to wait on, held across the
    // allocation's transactions
    let mut i = ns.lock();

    i.old_stripe_open = Some(h.into_claim());
    i.old_stripe.set(s);

    init_new_stripe_from_old(&mut i, carry, move_, true);

    stack_pin_init!(let cl = StackClosure::new());

    // A narrower stripe than the one it replaces carries more parity per data
    // block: new space, so reserve it here, where a full filesystem fails with
    // -ENOSPC, rather than NOFAIL once the buckets are ours. A same-width
    // repair replaces parity with as much parity and keeps its NOFAIL
    // reservation below.
    let narrowing = nr_new_data < nr_data;

    let ret = StripeBuf::new_whole(fs, i.new_stripe.stripe(), MemWait::Never)
        .map(|buf| i.new_buf = Some(buf))
        .and_then(|()| if narrowing { i.reserve_parity(ns.fs_ref(), c::bch_reservation_flags::empty()) } else { Ok(()) })
        .and_then(|()| lockrestart_do(trans, |t| stripe_idx_alloc(t, &mut i)));
    if let Err(e) = ret {
        drop(i);
        discard(new_stripes_take(fs, p));
        return Err(e);
    }

    let target = if v.disk_label != 0 { Target::Group(u32::from(v.disk_label) - 1).encode().into() } else { 0 };

    let ret = lockrestart_do(trans, |t| {
        let dev_stripe = dev_stripe_state_get(t, v.disk_label.into())?;
        let mut req = t.alloc_request(target, false, core::ptr::null_mut(), 0, 0,
                                      c::bch_watermark::BCH_WATERMARK_normal, c::bch_write_flags::empty(), cl.as_ptr())?;
        match new_stripe_alloc_buckets(t, &mut req, dev_stripe, &mut i, ns.fs_ref(), true) {
            Err(e) if e.matches(bch_errcode::BCH_ERR_operation_blocked) => {
                t.wait_on_allocator(&mut req, e, &cl)?;
                Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested))
            }
            ret => ret,
        }
    });

    if let Err(e) = ret {
        let mut msg = log_msg_ratelimited!(fs);
        write!(msg, "stripe repair: error allocating buckets for the new stripe: {e}");
        prt!(&mut msg, "\nold: {}", i.old_stripe.stripe().to_text(fs));
        prt!(&mut msg, "\nnew: {}", i.new_stripe.stripe().to_text(fs));
        drop(msg);

        drop(i);
        discard(new_stripes_take(fs, p));
        return Err(e);
    }

    trans.unlock_long();

    if let Err(e) = i.buckets_add(fs) {
        drop(i);
        discard(new_stripes_take(fs, p));
        return Err(e);
    }
    i.allocated = true;

    if !narrowing {
        // NOFAIL: it doesn't fail
        let _ = i.reserve_parity(ns.fs_ref(), c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL);
    }

    // As ec_old_stripe_read(), for an old stripe the caller has already read,
    // with every block we carry good: ec_old_stripe_take().
    i.old_stripe_read = true;
    i.old_stripe_read_all = true;
    old_stripe_carry(&mut i, &mut read);
    i.old_buf = Some(read);

    let v = i.new_stripe.stripe().v();
    let stripe_sectors = u32::from(v.sectors.get()) * u32::from(v.nr_blocks);
    i.write = Some(ctxt.write_start(stripe_sectors));
    drop(i);

    // in new_stripes already: no head's
    ns.fill(&fs.ec.new_stripes.lock());
    Ok(())
}
