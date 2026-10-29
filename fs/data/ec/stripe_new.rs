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

//! Stripes being made - ec/create.c's ec_stripe_new: a new stripe's life,
//! from the stripe head that stages it (head.rs) to its creation. Its
//! blocks' buckets are stripe_new_blocks.rs's, the old stripe it reuses
//! stripe_new_reuse.rs's, and its task stripe_new_tasks.rs's; repair's
//! rebuild (repair.rs) makes one from the stripe it replaces.
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
//! Its io refs, IoRef - a count on the stripe (util/refcount.rs): one per
//! block a writer has claimed, leaked to C with the block's open bucket and
//! put as it closes - foreground.c's __bch2_open_bucket_put() - and one of its
//! own, its head's or repair's, put by fill(). The last put takes it in
//! flight, there and then - its seq assigned, the flushes woken - as C's did:
//! a flush must see a stripe in flight as soon as its writers are done, not
//! when its task next runs. It then puts the one ref on @in_flight, the
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
//! anyone's; and its internal state, Internal, under its lock (lock()),
//! which publishes its blocks' devices (BlockDevs).
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

use crate::alloc::buckets::bucket_to_u64;
use crate::alloc::capacity::DiskReservation;
use crate::alloc::foreground::{EcOpenBucket, OpenBucket, OpenBucketRef};
use crate::btree::bkey::{pos, BkeyV};
use crate::c;
use crate::data::ec::buf::StripeBuf;
use crate::data::ec::open::StripeClaim;
use crate::data::ec::stripe::{StripeKey, MAX_BLOCKS};
use crate::data::ec::stripe_new_blocks::new_stripe_key;
use crate::data::ec::stripe_new_reuse::Reuse;
use crate::data::ec::stripe_new_tasks::stripe_task;
use crate::data::moving::MovingIo;
use crate::errcode::{bch_errcode, BchError, FirstErr};
use crate::fs::Fs;
use crate::locking::{Guard, Lock};
use crate::util::alloc::{kbox_pin_init, AllocError, KBox, KVVec};
use crate::util::alloc::flags::GFP_KERNEL;
use crate::util::backref::BackRef;
use crate::util::ffi::Opaque;
use crate::util::gather::{Gather, GatherRefSlot};
use crate::util::refcount::{RefGuard, Refcount};
use crate::util::kernel::local_clock;
use crate::util::sleep::Sleep;
use crate::util::task::{spawn_unstarted, Spawn, Unstarted};
use crate::util::projections::Volatile;
use crate::util::Printbuf;
use crate::{new_lock, published, prt};
use arrayvec::ArrayVec;
use core::ffi::c_void;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use cstruct_macros::c_export;
use pin_init::{pin_data, pin_init, pinned_drop, PinInit};
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
    pub(crate) idx:                  AtomicU64,

    io_refs:              AtomicI32,
    /// The io refs' ref on @in_flight, put by the last: before it, as a
    /// Gather's drop waits for its refs
    in_flight_ref:        GatherRefSlot<()>,
    /// What its task waits on to be in flight
    #[pin]
    pub(crate) in_flight:            Gather<()>,
    seq:                  AtomicU64,
    /// Why it's cancelled - a writer's IO failed, or its head gave up on it:
    /// the first error
    pub(crate) err:       FirstErr,
    /// An ec_stripe_new_state
    state:                AtomicU32,

    #[pin]
    internal:             Lock<c::mutex, Internal, BlockDevs>,
}

published! {
    /// The devices its blocks' new buckets are on, for stopping a device -
    /// under the allocator's spinlocks, where its lock can't be waited for.
    /// A bucket's device is set as it's given a block, and never cleared: one
    /// moved or dropped leaves its bit, and a stop drops a bucket it needn't
    /// have, as stopping the filesystem drops them all.
    #[derive(::pin_init::Zeroable)]
    pub(crate) struct BlockDevs {
        pub(crate) block_devs: Volatile<c::DevsMask>,
    }
}

impl BlockDevsWriter {
    /// @dev, given a block.
    pub(crate) fn add(&self, dev: u32) {
        let mut devs = self.block_devs.get();
        devs.set(dev as usize);
        self.block_devs.set(devs);
    }
}

/// A stripe being made's lock, held: its internal state, and the writer of
/// what it publishes.
pub(crate) type InternalGuard<'a> = Guard<'a, c::mutex, Internal, BlockDevs>;

/// What a stripe being made holds on one of its blocks' buckets: a pin, or -
/// once a writer's claimed the block - the bucket's ->ec. Given back when
/// it's dropped: the pin put, the claimed bucket closed.
pub(crate) enum StripeBlock {
    Pinned(OpenBucket),
    Claimed(EcOpenBucket),
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

/// A block of a stripe being made, by what it holds: a new bucket, or a block
/// of the old stripe it reuses - carried in place, or moved to a new bucket.
/// What C kept as blocks_gotten, blocks_allocated, blocks_moving, blocks[]
/// and old_block_map, each a view of this.
///
/// The old stripe's blocks are a prefix, in order: init_new_stripe_from_old()
/// lays them out, so the map to them - old_block_map() - is the slots up to
/// the first that isn't one.
#[derive(Default)]
pub(crate) enum Slot {
    /// Nothing yet: a bucket's to be allocated for it.
    #[default]
    Empty,
    /// A new bucket: Pinned until a writer claims it - only data blocks'
    /// are - then Claimed.
    Bucket(StripeBlock),
    /// Block @old of the old stripe, in place: its bucket is the old
    /// stripe's, so no open bucket of ours.
    Carried { old: u8 },
    /// Block @old of the old stripe, its data to a new bucket - once it's
    /// allocated, Pinned, and never a writer's.
    Moving { old: u8, bucket: Option<StripeBlock> },
}

impl Slot {
    /// Whether it has its block: a bucket, or the old stripe's in place.
    pub(crate) fn gotten(&self) -> bool {
        !matches!(self, Slot::Empty | Slot::Moving { bucket: None, .. })
    }

    /// Whether it's no writer's to be given: one has it, or it's the old
    /// stripe's.
    pub(crate) fn allocated(&self) -> bool {
        matches!(self, Slot::Bucket(StripeBlock::Claimed(_)) | Slot::Carried { .. } | Slot::Moving { .. })
    }

    pub(crate) fn moving(&self) -> bool {
        matches!(self, Slot::Moving { .. })
    }

    /// Its bucket, if it's a new one.
    pub(crate) fn bucket(&self) -> Option<&StripeBlock> {
        match self {
            Slot::Bucket(b) | Slot::Moving { bucket: Some(b), .. } => Some(b),
            _ => None,
        }
    }

    /// The old stripe's block it carries forward, in place or moved.
    pub(crate) fn old(&self) -> Option<u8> {
        match *self {
            Slot::Carried { old } | Slot::Moving { old, .. } => Some(old),
            _ => None,
        }
    }

    /// A new bucket, for a slot that hasn't got its block.
    pub(crate) fn fill(&mut self, b: StripeBlock) {
        match self {
            Slot::Empty => *self = Slot::Bucket(b),
            Slot::Moving { bucket: bucket @ None, .. } => *bucket = Some(b),
            _ => panic!("filling a stripe block that has its bucket"),
        }
    }
}

#[pin_data]
pub(crate) struct Internal {
    /// A repair's: its writes, in flight in the repair's moving context
    pub(crate) write:                Option<MovingIo>,
    /// Its task, until it's started
    task:                 Option<Unstarted<()>>,
    /// Its maker's io ref - its head's or repair's - till fill() puts it
    maker:                Option<IoRef>,

    pub(crate) devs:      c::bch_devs_mask,
    pub(crate) watermark: c::bch_watermark,

    pub(crate) allocated:           bool,

    pub(crate) blocks:           [Slot; MAX_BLOCKS],
    /// Its parity's, once reserve_parity()'s made it
    pub(crate) res:              Option<DiskReservation<'static>>,

    /// Its buckets in ec.stripes_new_buckets: buckets_add()'s
    buckets:                     ArrayVec<u64, MAX_BLOCKS>,

    /// Its key: at its slot, once it has one
    pub(crate) new_stripe:       StripeKey,
    pub(crate) new_buf:          Option<KBox<StripeBuf>>,
    /// Its slot, held open: stripe_idx_alloc()'s
    pub(crate) slot_claim:       Option<StripeClaim>,

    /// The old stripe it reuses, if it does
    pub(crate) reuse:            Option<KBox<Reuse>>,
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
    pub(crate) fn reserve_for_head(&mut self) -> Result<(), AllocError> {
        self.v.reserve(self.heads_held + 1, GFP_KERNEL)?;
        self.heads_held += 1;
        Ok(())
    }

    /// A head's stripe is gone without being handed on: its room is no
    /// longer kept.
    pub(crate) fn release_for_head(&mut self) {
        self.heads_held = self.heads_held.checked_sub(1).expect("a head's stripe, room kept for it");
    }

    /// A head's stripe, handed on: into the room kept for it, so this can't
    /// fail.
    pub(crate) fn push_from_head(&mut self, b: StripeNewBox) {
        self.release_for_head();
        assert!(self.v.len() < self.v.capacity(), "no room kept for a head's stripe");
        let Ok(()) = self.v.push(b, GFP_KERNEL) else { unreachable!("pushed within capacity") };
    }

    /// A repair's stripe, which isn't a head's: past the room kept for them.
    pub(crate) fn push(&mut self, b: StripeNewBox) -> Result<(), (AllocError, StripeNewBox)> {
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


impl StripeNew {
    /// A new stripe of @nr_data data blocks and @nr_parity parity of
    /// @blocksize sectors, in @disk_label, by @algorithm, on @devs - and its
    /// task, made - open: ec_new_stripe_alloc(). Its one io ref is its
    /// maker's, in @maker.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn alloc(fs: &Fs, devs: c::bch_devs_mask, watermark: c::bch_watermark, disk_label: u32, algorithm: u32,
             nr_data: u32, nr_parity: u32, blocksize: u32) -> Result<StripeNewBox, BchError> {
        let b = kbox_pin_init(pin_init!(StripeNew {
            // SAFETY: its head and new_stripes, which own it, are emptied
            // going read-only
            fs:         unsafe { BackRef::new(fs) },
            start_time: local_clock(),
            idx:        AtomicU64::new(0),
            io_refs:    AtomicI32::new(0),
            in_flight_ref: GatherRefSlot::new(),
            in_flight   <- Gather::new(),
            seq:        AtomicU64::new(0),
            err:        FirstErr::new(),
            state:      AtomicU32::new(State::EC_STRIPE_NEW_open as u32),
            internal    <- new_lock!(<- Internal::new(devs, watermark), ::pin_init::zeroed(), "stripe_new.internal"),
        }? BchError))?;

        // SAFETY: its owner - its head, then new_stripes - holds it until its
        // task's teardown(), which drops this first; or drops it, its task
        // unstarted and this unused, in discard(). Its state's interior: it's
        // shared from here, and only discard()'s drop has it by &mut
        let p = unsafe { BackRef::new(&*b) };
        {
            let mut s = b.lock();
            new_stripe_key(fs, &mut s.new_stripe, algorithm as u8, nr_data as u8, nr_parity as u8,
                           blocksize as u16, disk_label as u8);

            // SAFETY: what the task borrows outlives it - see the file comment
            s.task = Some(unsafe { spawn_unstarted(Spawn::Queue(&fs.ec.stripe_create_wq), move || stripe_task(fs, p)) }?);
            s.maker = Some(IoRef::get(&b));
        }

        b.in_flight_ref.set(b.in_flight.get());
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

    /// Its filesystem, for reserve_parity().
    pub(crate) fn fs_ref(&self) -> &BackRef<Fs> {
        &self.fs
    }

    /// Never held across an await
    /// - a mutex is released by the task that took it, and its task resumes
    /// on any worker - and, once it's launched, never long.
    pub(crate) fn lock(&self) -> InternalGuard<'_> {
        self.internal.lock()
    }

    fn state(&self) -> State {
        State::from_bits(self.state.load(Ordering::Relaxed))
    }

    /// Its task started, unless it has been, and its slot published - @s its
    /// internal state, locked: from here on, writers may have it.
    pub(crate) fn launch(&self, s: &mut Internal) {
        if let Some(t) = s.task.take() {
            self.idx.store(s.new_stripe.idx(), Ordering::Relaxed);
            // detached: it removes itself from new_stripes when it's done
            drop(t.start());
        }
    }

    /// Filling: its task started, and its maker's io ref put - the maker's last
    /// touch of it. @_v, new_stripes, locked, and it's in them: so its task,
    /// which takes it out to free it, can't before we're done.
    pub(crate) fn fill(&self, _v: &NewStripes) {
        self.state.store(State::EC_STRIPE_NEW_filling as u32, Ordering::Relaxed);
        let maker = {
            let mut i = self.lock();
            self.launch(&mut i);
            i.maker.take().expect("a stripe handed on twice")
        };
        drop(maker);
    }
}

/// A stripe being made's io refs: see the file comment.
pub(crate) enum IoRefs {}

/// An io ref on a stripe being made.
pub(crate) type IoRef = RefGuard<StripeNew, IoRefs>;

// SAFETY: @io_refs counts them. The last dec() puts the io refs' ref on
// @in_flight, the last touch: the task may free the stripe after it - while
// dec()'s &self is still on the stack, as Gather::put()'s is.
unsafe impl Refcount<IoRefs> for StripeNew {
    fn inc(&self) {
        self.io_refs.fetch_add(1, Ordering::Relaxed);
    }

    /// The last takes it in flight - its seq the next, the flushes woken -
    /// and has its task go on to create it.
    unsafe fn dec(&self) {
        let fs = &*self.fs;
        let v = self.io_refs.fetch_sub(1, Ordering::AcqRel);
        assert!(v > 0, "stripe {} io ref put with none held", self.idx.load(Ordering::Relaxed));
        if v != 1 {
            return;
        }

        // Every bucket is back: the data is all in. This is the filling ->
        // in_flight transition, after which the stripe completes without
        // needing anything from a writer; seq is the commit-ready marker
        // fs_ec_flush_outstanding() waits on.
        self.state.store(State::EC_STRIPE_NEW_in_flight as u32, Ordering::Relaxed);
        self.seq.store(fs.ec.stripe_new_seq.fetch_add(1, Ordering::Relaxed) as u64 + 1, Ordering::Release);
        fs.ec.new_stripes_changed.wake();
        self.in_flight_ref.take().expect("the io refs' ref on its in flight, put once").put(());
    }
}

impl Internal {
    /// Open, on @devs, at @watermark: its keys zeroed, for new_stripe_key().
    fn new(devs: c::bch_devs_mask, watermark: c::bch_watermark) -> impl PinInit<Self> {
        pin_init!(Internal {
            write:               None,
            task:                None,
            maker:               None,
            devs,
            watermark,
            allocated:           false,
            blocks:              Default::default(),
            res:                 None,
            buckets:             ArrayVec::new(),
            new_stripe           <- pin_init::init_zeroed(),
            new_buf:             None,
            slot_claim:          None,
            reuse:               None,
        })
    }

    pub(crate) fn nr_data(&self) -> u32 {
        self.new_stripe.stripe().v().nr_data()
    }

    /// Its blocks that have one: a bucket, or the old stripe's in place.
    pub(crate) fn gotten(&self) -> c::StripeBlockMask {
        let mut m = c::StripeBlockMask::default();
        for (i, b) in self.blocks.iter().enumerate() {
            if b.gotten() {
                m.set(i);
            }
        }
        m
    }

    /// The old stripe's blocks it carries forward, by new block: block i is
    /// the old stripe's old_block_map()[i].
    pub(crate) fn old_block_map(&self) -> ArrayVec<u8, MAX_BLOCKS> {
        self.blocks.iter().map_while(Slot::old).collect()
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

        for b in self.blocks[..nr].iter().filter_map(Slot::bucket) {
            let id = b.id();
            let bucket = bucket_to_u64(pos(id.dev().into(), id.bucket()));

            match fs.ec.stripes_new_buckets.insert_new(bucket) {
                Ok(Ok(())) => self.buckets.push(bucket),
                Ok(Err(_)) => panic!("bucket {}:{} in two stripes being made", id.dev(), id.bucket()),
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
        assert!(s.maker.is_none(), "stripe {} freed holding its maker's io ref", self.idx.load(Ordering::Relaxed));

        s.blocks = Default::default();
        s.res = None;
        s.buckets_del(fs);

        if let Some(claim) = s.slot_claim.take() {
            claim.put(fs);
        }
        if let Some(r) = s.reuse.take() {
            r.put(fs);
        }
        drop(s.write.take());
    }
}

/// @b, never launched, freed: a repair's rebuild that failed, or a head's
/// stripe at shutdown. Only its own io ref is held - no writer has a block -
/// and its task, never started, goes with it.
pub(crate) fn discard(b: StripeNewBox) {
    assert!(b.lock().task.is_some(), "discarding stripe {} with its task started", b.idx.load(Ordering::Relaxed));
    // given up without its put: never launched, it's not taken in flight
    b.lock().maker.take().expect("discarding a stripe handed on").leak();
    let v = b.io_refs.swap(0, Ordering::Relaxed);
    assert_eq!(v, 1, "discarding stripe {} with writers' io refs", b.idx.load(Ordering::Relaxed));
    b.in_flight_ref.take().expect("the io refs' ref on its in flight").put(());
    drop(b);
}

pub(crate) fn new_stripes_take(fs: &Fs, p: NonNull<StripeNew>) -> StripeNewBox {
    fs.ec.new_stripes.lock().take(p)
}

// ── Its writers' ───────────────────────────────────────────────────────────

impl OpenBucketRef {
    /// The stripe being made it's a block of, if a writer's claimed it for
    /// one.
    pub fn stripe(&self) -> Option<&StripeNew> {
        let s = self.ec.load(Ordering::Relaxed);
        // SAFETY: its ->ec holds an io ref on the stripe while it's open,
        // which it is while it's borrowed
        (!s.is_null()).then(|| unsafe { StripeNew::from_c(s) })
    }
}

impl StripeNew {
    /// A writer's io ref put: its open bucket's, at its last pin's put.
    ///
    /// # Safety
    /// The caller has the io ref - claim_bucket()'s, leaked to the open
    /// bucket's ->ec - and gives it up.
    pub(crate) unsafe fn put_io(&self) {
        // SAFETY: the caller's
        drop(unsafe { IoRef::adopt(self) });
    }

    /// Whether it has a block on device @dev - or had: BlockDevs.
    pub(crate) fn has_dev(&self, dev: u32) -> bool {
        self.internal.block_devs.get().test(dev as usize)
    }
}

/// Where @wp's write goes in its stripe's buffer, if one of its open buckets
/// is a block of a stripe being made: NULL if none is.
///
/// # Safety
/// @wp is a write point, locked.
#[c_export]
pub unsafe fn bch2_writepoint_ec_buf(c: &Opaque<c::bch_fs>, wp: &c::write_point) -> *mut c_void {
    let fs = Fs::from_c(c);
    // SAFETY: the caller's
    let wp = unsafe { wp.alloc.lock.held() };
    // the write point's pins, which its lock keeps
    let Some((ob, s)) = wp.ptrs.iter(fs).find_map(|ob| Some((ob, ob.stripe()?))) else {
        return core::ptr::null_mut()
    };

    let offset = u32::from(ob.ca().mi.get().bucket_size) - ob.sectors_free.get();
    let mut s = s.lock();
    let buf = s.new_buf.as_mut().expect("a stripe with blocks claimed has its buffer");
    // its writer's to write, at its write point's free space
    buf.block_mut(usize::from(ob.ec_idx.get()))[(offset as usize) << 9..].as_mut_ptr().cast()
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
    /// (stripe_heads_stop(), say) cancels them before this.
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
         i.blocks[..v.nr_data() as usize].iter().filter(|b| b.allocated()).count(),
         s.io_refs.load(Ordering::Relaxed), i.watermark, s.state());

    for b in i.blocks[..usize::from(v.nr_blocks)].iter().filter(|b| b.gotten()) {
        prt!(out, " {}", b.bucket().map_or(0, |ob| ob.idx().get()));

        let Slot::Bucket(StripeBlock::Claimed(ob)) = b else { continue };
        let bucket_size = u32::from(ob.ca().mi.get().bucket_size);
        prt!(out, "(pin {} {}/{})", ob.pin.load(Ordering::Relaxed), bucket_size - ob.sectors_free.get(), bucket_size);
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

        crate::data::ec::head::stripe_heads_to_text(out, self);

        prt!(out, "in flight:\n");
        for s in self.ec.new_stripes.lock().iter().filter(|s| s.state() != State::EC_STRIPE_NEW_open) {
            new_stripe_to_text(out, self, s);
        }
    }
}
