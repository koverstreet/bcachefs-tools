// SPDX-License-Identifier: GPL-2.0
// Copyright 2010, 2011 Kent Overstreet <kent.overstreet@gmail.com>
// Copyright 2012 Google, Inc.

/* DOC(data-read-path)
 *
 * Reads are transparent and self-healing: if a checksum failure or IO error
 * occurs on one replica, bcachefs automatically retries from another replica.
 * The failed device's error counter is incremented and the bad copy is
 * rewritten from the good one. If all replicas fail, the error is propagated
 * to the application.
 *
 * With multiple devices, reads go to the lowest-latency replica. This is
 * tracked per-device and adapts over time, so mixed SSD/HDD configurations
 * automatically prefer the SSD for reads without explicit configuration.
 *
 * End-to-end flow: extent lookup, device selection, disk read, checksum
 * verification, decryption, decompression. For compressed or checksummed
 * extents the full extent must be read even for partial requests, because
 * checksums and compression operate on the whole extent.
 */

//! The read path: a range of a file, or one extent, read into a read bio - a
//! replica picked, read, and what it read checked, decrypted, and decompressed
//! or bounced into the caller's bio.
//!
//! read() reads a range of a file into a read bio, and is awaited. A data
//! update's read, read_update(), reads its one fragment in the update's own
//! task, and writes nothing of the update but its rbio.
//!
//! A read walks the extents of its range, and each extent's part of it is a
//! fragment. fragment_of() picks the replica in the caller's transaction, and
//! what it picked is the fragment's Plan - its bio not made yet. read() starts
//! each fragment but the read's own bio's as a task in a box of its own, from
//! the module's pool: the task, its read bio and the bio's bvecs in one
//! allocation, freed a grace period after the task's done (Planned::start()),
//! run inline till it waits on the IO; resume_target() says where the work
//! after it runs. The read's own bio's - the read isn't split - is the walk's
//! last, and read() reads it once its walk is done. A retry's fragments, and a
//! data update's, are of the filesystem's bioset, read in the caller's task
//! (Planned::make()).
//!
//! Each fragment's task puts what it read to read()'s Gather, which read()
//! awaits once its walk is done - having given back first what's the
//! filesystem's, as once it's put the read and the filesystem may go. The
//! first error put is the read's; the walk's own - an extent it couldn't read
//! - are in the read's ret.
//!
//! A fragment that needs a reconstruct, or fails with a retryable error, goes
//! to a retry (Fragment::end()), awaited in its task: its future in a slot of
//! read_retry_pool, a mempool, so a retry doesn't fail for memory and a
//! fragment's future isn't the size of one. It rereads the range - the walk
//! again, each fragment awaited, a transaction per step, as BtreeTrans is
//! !Send - and reports what failed and how it came out. A retry's walk is one
//! with failures to track, and that's what it does differently: a stale dirty
//! pointer is an inconsistency, not a race; no checksums are narrowed; the
//! retry reports, not each fragment.
//!
//! A promote's fragment reads into the promote's own read bio, lent it, and
//! its task's box has no bio of its own: the promote's task is started with
//! what was read as soon as that bio's given back, the fragment's task going
//! on in its own memory.

use crate::alloc::{background::bucket_read_time_reset, capacity::DiskReservation};
use crate::btree::bkey::{pos, prelude::*, BkeyI, BkeySC};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{commit_do, is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
                         LoopFlow, TransAttempt, UpdateTriggerFlags};
use crate::{bch_err, btree_trans, c, fs_inconsistent, log_msg_ratelimited, prt};
use crate::data::{compress::bio_uncompress, ec, inline_data::{self, CsumMismatch, InlineDataRef},
                  promote::{promote_alloc, Promoted}, update::read_err_benign, update_task::AwaitingRead};
use crate::data::ptrs::{crc_is_compressed, extent_nonce, BkeyVPtrs, BkeyVRead, Ptrs, PtrsMut};
use crate::data::reflink::{lookup_indirect_extent, Indirect};
use crate::errcode::{Errcode as err, ret_to_c, ret_to_result_void, BchError};
use crate::fs::{Dev, Fs};
use crate::init::{damage, error::{bch2_inum_offset_err_msg_trans, inum_offset_err_msg}};
use crate::inode_opts::get_io_opts;
use crate::locking::{Locked, MemallocFlags};
use crate::debug::trace::events;
use crate::sb::counters::{Event, Sectors};
use crate::init::error::id;
use crate::sb::errors::{bch2_decompress_sb_err, SbErrorId};
use crate::sb::members::DevIoRef;
use crate::snapshots::subvolume::{for_each_in_subvolume_max, get_snapshot};
use crate::util::backref::BackRef;
use pin_init::{pin_data, pin_init, pinned_drop, stack_pin_init, PinInit};
use crate::util::bio::{inline_vecs, reset_bvec_offset, Bio, BioBox, BioBuffersOwned, BioBuffersUnmanaged, BioClone,
                       BioInPlace, BioLend, BioOwner, BioRange, Buffers, HasBio, Lent};
use crate::util::gather::{Gather, GatherRef};
use crate::os::kernel::{local_clock, READ};
use crate::util::alloc::{AllocBox, AllocError, Kmalloc};
use crate::util::mempool::Mempool;
use crate::util::rcu::read_lock;
use crate::util::time_stats::TimeStats;
use crate::util::task::{exclusive_size, requeue, waiter, Join, Resumable, Resume, ResumeAt, TaskSlot};
use crate::debug::async_objs::{AsyncObj, AsyncObjSlot};
use crate::util::workqueue::Queue;
use crate::util::{Nsecs, Printbuf, ToText};
use core::ffi::c_uint;
use core::sync::atomic::{AtomicBool, AtomicI16, Ordering::Relaxed};
use core::ops::ControlFlow;
use core::alloc::Layout;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use crate::types::c_default;
use cstruct_macros::{args_stored_once, bch_enum, in_place, MaybeZeroable, Init, TypeInfo};
use crate::format::{Bpos, Btree, Bversion, DataType, MetadataVersion, Nonce};

type F = c::ReadFlags;

#[allow(non_upper_case_globals)]
pub const BCH_READ_ERR_checksum: u32 = 1 << 0;
#[allow(non_upper_case_globals)]
pub const BCH_READ_ERR_io: u32 = 1 << 1;
#[allow(non_upper_case_globals)]
pub const BCH_READ_ERR_decompression: u32 = 1 << 2;
#[allow(non_upper_case_globals)]
pub const BCH_READ_ERR_ec_reconstruct: u32 = 1 << 3;

#[bch_enum(closed)]
#[repr(u32)]
pub enum ReadFor {
    BCH_READ_FOR_read,
    BCH_READ_FOR_update,
    BCH_READ_FOR_scrub,
}

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct ReadErrs {
    pub errors: u32,
    pub msg:    Printbuf,
}

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct ReadErrReport {
    #[pin]
    #[init(crate::new_lock!(ReadErrs { errors: 0, msg: Printbuf::new() }, "bch_read_err_report"))]
    pub errs: Locked<c::mutex, ReadErrs>,
}

#[pin_data]
#[derive(MaybeZeroable, TypeInfo)]
#[repr(C)]
pub struct Rbio {
    pub c: *mut Fs,
    pub start_time: u64,

    pub saw_error: AtomicBool,
    pub self_healing: AtomicBool,
    pub ret: AtomicI16,
    pub read_for: ReadFor,

    pub subvol: u32,
    pub opts: c::bch_inode_opts,

    pub err_report: Option<BackRef<ReadErrReport>>,

    #[pin]
    pub bio: BioLend,
}
c_default!(Rbio);
crate::has_bio!(Rbio, bio);

unsafe impl Send for Rbio {}
unsafe impl Sync for Rbio {}

impl Rbio {
    pub fn ret(&self) -> Result<(), BchError> {
        ret_to_result_void(self.ret.load(Relaxed).into())
    }

    pub fn set_ret(&self, ret: Result<(), BchError>) {
        self.ret.store(ret_to_c(ret) as i16, Relaxed);
    }

    pub(crate) fn fs(&self) -> &Fs {
        unsafe { Fs::borrow_raw(self.c) }
    }

    fn report(&self, errors: u32, msg: impl Fn(&mut Printbuf)) -> bool {
        let Some(r) = &self.err_report else { return false };
        let mut e = r.errs.lock();
        e.errors |= errors;
        msg(&mut e.msg);
        true
    }

    fn is_update(&self) -> bool {
        self.read_for != ReadFor::BCH_READ_FOR_read
    }

    fn decodes(&self) -> bool {
        self.read_for != ReadFor::BCH_READ_FOR_update
    }

    pub fn new<'a>(fs: &'a Fs, opts: c::bch_inode_opts, subvol: u32, err_report: Option<&'a ReadErrReport>,
                   bio: BioInPlace) -> impl PinInit<Self, BchError> + 'a {
        let err_report = err_report.map(|r| unsafe { BackRef::new(r) });
        Self::with(fs.as_ptr(), local_clock(), ReadFor::BCH_READ_FOR_read, subvol, opts, err_report, bio)
    }

    /// A data update's read bio.
    pub(crate) fn update(fs: &Fs, opts: c::bch_inode_opts, read_for: ReadFor, bio: BioInPlace)
        -> impl PinInit<Self, BchError>
    {
        Self::with(fs.as_ptr(), local_clock(), read_for, 0, opts, None, bio)
    }

    /// The bio of a fragment of @orig's read: what the read is - its
    /// subvolume, options, error report - stays @orig's, and is read there.
    fn fragment(orig: &Rbio, bio: BioInPlace) -> impl PinInit<Self, BchError> + '_ {
        Self::with(orig.c, orig.start_time, orig.read_for, 0, c::bch_inode_opts::default(), None, bio)
    }

    fn with(c: *mut Fs, start_time: u64, read_for: ReadFor, subvol: u32, opts: c::bch_inode_opts,
            err_report: Option<BackRef<ReadErrReport>>, bio: BioInPlace) -> impl PinInit<Self, BchError> {
        pin_init!(Rbio {
            c,
            start_time,
            saw_error:    AtomicBool::new(false),
            self_healing: AtomicBool::new(false),
            ret:          AtomicI16::new(0),
            read_for,
            subvol,
            opts,
            err_report,
            bio           <- BioLend::new::<BchError>(bio),
        }? BchError)
    }

    /// Only called on a top level bch_read_bio to complete an entire read
    /// request, not a split.
    fn done(&self) {
        if self.start_time != 0 {
            self.fs().times[c::TimeStat::data_read as usize].update(self.start_time);
        }
    }

    #[cold]
    fn state_to_text(&self, out: &mut Printbuf, flags: Option<F>) {
        if out.nr_tabstops() == 0 {
            out.tabstop_push(20);
        }
        let mut out = out.indent(2);
        let now = local_clock();

        prt!(out, "start_time:\t{}\n", Nsecs(now.saturating_sub(self.start_time)));

        match self.ret() {
            Ok(()) => write!(out, "ret:\t0\n"),
            Err(e) => write!(out, "ret:\t{e}\n"),
        }

        if let Some(flags) = flags {
            write!(out, "flags:\t");
            flags.to_text(&mut out);
            out.newline();
        }

        self.bio().to_text(&mut out);
    }

    #[cold]
    fn to_text_atomic(&self, out: &mut Printbuf, read_pos: Bpos, flags: F) {
        write!(out, "{read_pos}\n");
        self.state_to_text(out, Some(flags));
    }

    #[cold]
    pub fn to_text(&self, out: &mut Printbuf) {
        self.state_to_text(out, None);
    }
}

unsafe impl AsyncObj for Rbio {
    const LIST: c::AsyncObjKind = c::AsyncObjKind::rbio;

    #[cold]
    fn to_text(&self, out: &mut Printbuf, _: &Fs) {
        Rbio::to_text(self, out)
    }
}

pub struct ReadDone {
    pub ret:           Result<(), BchError>,
    pub pick:          Option<c::extent_ptr_decoded>,
    pub ptrs_io_error: u16,
    pub poisoned:      Option<BkeyBuf>,
}

#[derive(Clone, Copy)]
struct Plan<'r> {
    range:              BioRange<'r>,
    flags:              F,
    pick:               c::extent_ptr_decoded,
    read_pos:           Bpos,
    data_btree:         Btree,
    data_pos:           Bpos,
    version:            Bversion,
    offset_into_extent: u32,
    narrow_crcs:        bool,
    /* what it reads is in the read's bio: an update's read_ptr, read_crc */
    landed:             bool,
}

struct LentBio<'r> {
    rbio: &'r Rbio,
    bio:  Lent<'r>,
}

impl Rbio {
    fn lend(&self) -> LentBio<'_> {
        LentBio { rbio: self, bio: self.bio.lend() }
    }
}

enum Kind<'r> {
    Own(LentBio<'r>),
    Clone(BioBox<Rbio, BioClone<'r>>),
    Bounce(BioBox<Rbio, BioBuffersOwned<'r>>),
    /// The promote's bio, and its task, started with what was read once the
    /// bio's given back
    Promote(Lent<'r>, AwaitingRead<'r>),
    /// A fragment's task's own bio, in its home: a clone, or bounced into the
    /// pages it has
    Home(Pin<&'r mut FragmentBio>, Option<BioBuffersOwned<'r>>),
}

impl Kind<'_> {
    fn rbio(&self) -> &Rbio {
        match self {
            Kind::Own(b) => b.rbio,
            Kind::Promote(_, task) => task.rbio(),
            Kind::Clone(b) => b,
            Kind::Bounce(b) => b,
            Kind::Home(h, _) => &h.rbio,
        }
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        match self {
            Kind::Own(LentBio { bio, .. }) | Kind::Promote(bio, _) => bio.bio_mut(),
            Kind::Clone(b) => b.bio_mut(),
            Kind::Bounce(b) => b.bio_mut(),
            Kind::Home(h, _) => h.as_mut().bio_mut(),
        }
    }
}

/// A fragment's task's home: its read bio, the bvecs inline after its box.
#[pin_data(PinnedDrop)]
#[repr(C)]
struct FragmentBio {
    #[pin]
    rbio: Rbio,
}
crate::has_bio!(FragmentBio, rbio.bio);

#[pinned_drop]
impl PinnedDrop for FragmentBio {
    fn drop(self: Pin<&mut Self>) {
        unsafe { self.bio_mut().uninit() }
    }
}

/// A fragment's task, in place: freed when it's done, after its read is - so
/// from a pool of the module's, as the box may outlive the filesystem.
static FRAGMENTS: Mempool<Kmalloc> = ::pin_init::zeroed();

/// At module init: elements for a fragment's task, and the most bvecs a bounce
/// has.
pub fn pools_init() -> Result<(), BchError> {
    type Pool = &'static Mempool<Kmalloc>;
    let size = exclusive_size::<FragmentBio, FragmentFut<'static>, Pool>(inline_vecs(c::BIO_MAX_VECS as u16));
    unsafe { Mempool::init_static(&FRAGMENTS, Mempool::kmalloc(1, size)) }
}

/// # Safety
/// At module exit, after the last box's grace period.
pub unsafe fn pools_exit() {
    unsafe { FRAGMENTS.exit() }
}

struct Fragment<'r> {
    /* first, to be dropped before the bio it lists */
    listed: AsyncObjSlot,
    kind:   Kind<'r>,
    ca:     Option<DevIoRef<c::DevReadRefKind>>,
    plan:   Plan<'r>,
}

unsafe impl BioOwner for Fragment<'_> {
    type Buffers = BioBuffersUnmanaged;

    fn bio(&self) -> &Bio {
        self.rbio().bio()
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        self.kind.bio_mut()
    }
}

impl<'r> Fragment<'r> {
    fn new(fs: &Fs, kind: Kind<'r>, plan: Plan<'r>) -> Self {
        let listed = unsafe { AsyncObjSlot::new(fs, kind.rbio()) };
        Fragment { listed, kind, ca: None, plan }
    }

    fn rbio(&self) -> &Rbio {
        self.kind.rbio()
    }

    fn split(&self) -> bool {
        !matches!(self.kind, Kind::Own { .. })
    }

    fn bounced(&self) -> bool {
        matches!(self.kind, Kind::Bounce(_) | Kind::Promote(..) | Kind::Home(_, Some(_)))
    }

    #[cold]
    fn to_text(&self, out: &mut Printbuf) {
        self.rbio().to_text_atomic(out, self.plan.read_pos, self.plan.flags);
    }

    /// The fragment ended: done, or its retry made in its box - @range, if
    /// given, for the plan's - to be awaited.
    fn end(self, read: &'r Rbio, ret: Result<(), BchError>, at: Locate<'r>, range: Option<BioRange<'r>>)
        -> Result<Result<(), BchError>, RetryBox<'r>>
    {
        if self.plan.pick.do_ec_reconstruct || ret.is_err_and(should_retry) {
            Err(retry_boxed(read, self, ret, at, range))
        } else {
            self.free(ret);
            Ok(ret)
        }
    }

    fn end_in_retry(self, read: &Rbio, failed: &mut FailureState, ret: Result<(), BchError>)
        -> Result<(), BchError>
    {
        if self.plan.landed {
            failed.read = Some(self.plan.pick);
        }
        if let Err(e) = ret {
            read.saw_error.store(true, Relaxed);
            mark_failure(&self.plan.pick, &mut failed.io, e);
            failed_bio_to_text(&mut failed.io, self.bio(), e);
        }
        self.free(ret);
        ret
    }

    fn free(self, ret: Result<(), BchError>) {
        let Fragment { listed, kind, ca, plan } = self;
        drop(listed);
        drop(ca);

        let promote = match kind {
            Kind::Promote(bio, task) => {
                task.rbio().set_ret(ret);
                // given back before the promote's task, which writes from it, starts
                drop(bio);
                Some(task)
            }
            Kind::Home(mut h, Some(mut pages)) => {
                pages.release(h.as_mut().bio_mut());
                None
            }
            kind => {
                drop(kind);
                None
            }
        };
        if let Some(task) = promote {
            let read = ReadDone { ret, pick: Some(plan.pick), ptrs_io_error: 0, poisoned: None };
            unsafe { task.start(read) };
        }
    }
}

type Reads = Gather<Result<(), BchError>, waiter::Async>;
type ReadRef = GatherRef<Result<(), BchError>, waiter::Async>;

/// The filesystem's read_retry_pool: room for one retry's future, which
/// retry_boxed() makes in it.
pub(crate) fn retry_pool() -> impl PinInit<Mempool<Kmalloc>, BchError> {
    let layout = Layout::new::<RetryOwnedFut<'static>>();
    assert!(layout.align() <= 8, "a retry's future, aligned past a kmalloc()'s");
    Mempool::kmalloc(1, layout.size())
}

struct FailureState {
    io:       c::bch_io_failures,
    read:     Option<c::extent_ptr_decoded>,
    poisoned: Option<BkeyBuf>,
}

struct RetryState<'r> {
    plan:     Plan<'r>,
    ret:      Result<(), BchError>,
    failures: FailureState,
}

/// Where a walk is: the read bio's own bi_iter - which a folio added to the
/// bio grows - or, for a retry, the iter of what it rereads.
trait WalkPos {
    fn get(&self, read: &Rbio) -> c::bvec_iter;
    fn set(&mut self, read: &Rbio, iter: c::bvec_iter);
}

struct BioIter;

impl WalkPos for BioIter {
    fn get(&self, read: &Rbio) -> c::bvec_iter {
        read.bio().raw().bi_iter
    }

    fn set(&mut self, read: &Rbio, iter: c::bvec_iter) {
        unsafe { read.bio.lend().bio_mut().set_iter(iter) }
    }
}

impl WalkPos for c::bvec_iter {
    fn get(&self, _: &Rbio) -> c::bvec_iter {
        *self
    }

    fn set(&mut self, _: &Rbio, iter: c::bvec_iter) {
        *self = iter;
    }
}

/// After the Last step the walk's position isn't looked at: its bio may be in
/// flight.
#[derive(Clone, Copy, PartialEq)]
enum Step {
    More(u32),
    Last,
}

struct Walk<'a, P: WalkPos> {
    read:  &'a Rbio,
    pos:   P,
    flags: F,
    split: bool,
}

impl<'a, P: WalkPos> Walk<'a, P> {
    fn range(&self) -> BioRange<'a> {
        unsafe { self.read.bio().range_at(self.pos.get(self.read)) }
    }

    fn fragment(&mut self, e: &Extent<'_>) -> (BioRange<'a>, Step) {
        let range = self.range();
        let bytes = e.sectors.min(range.len() >> 9) << 9;
        if bytes == range.len() {
            (range, Step::Last)
        } else {
            self.split = true;
            (range.take(bytes), Step::More(bytes))
        }
    }

    fn step(&mut self, ret: Result<Step, BchError>) -> ControlFlow<Result<(), BchError>> {
        use ControlFlow::{Break, Continue};
        match ret {
            Ok(Step::Last) => Break(Ok(())),
            Ok(Step::More(bytes)) => {
                let mut range = self.range();
                range.advance(bytes);
                self.pos.set(self.read, range.pos());
                Continue(())
            }
            Err(e) => {
                if e == err::BCH_ERR_data_read_retry_csum_err_maybe_userspace.into() {
                    self.flags |= F::must_bounce;
                }
                if !should_retry(e) {
                    return Break(Err(e));
                }
                Continue(())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Locate<'a> {
    Inode(c::subvol_inum),
    Key(&'a BkeyI, Btree),
}

/// An extent being read: @k, in @btree - the reflink btree's, for an extent
/// that's indirect - read from @read_pos, @offset sectors into it, for
/// @sectors.
#[derive(Clone, Copy)]
struct Extent<'a> {
    read_pos: Bpos,
    btree:    Btree,
    k:        BkeySC<'a>,
    offset:   u32,
    sectors:  u32,
}

fn locate<'a>(t: &TransAttempt<'a, '_>, at: Locate<'a>, range: BioRange<'_>) -> Result<Extent<'a>, BchError> {
    let (key, btree) = match at {
        Locate::Inode(inum) => {
            let mut iter = BtreeIter::new(t, Btree::extents, pos(inum.inum, range.pos().bi_sector),
                                          BtreeIterFlags::SLOTS);
            iter.set_snapshot(get_snapshot(t, inum.subvol as u32)?);
            let read_pos = iter.pos();
            let k = iter.peek_slot(t)?.expect("peek_slot() returns a key");
            return extent_of(t, read_pos, k);
        }
        Locate::Key(key, btree) => (BkeySC::from(key), btree),
    };
    let mut it = BtreeIter::new(t, btree, key.k.start_pos(), BtreeIterFlags::empty());
    if !it.peek_slot(t)?.expect("peek_slot() returns a key").and_val_eq(key) {
        return Err(t.fs().err(err::BCH_ERR_data_read_key_overwritten));
    }
    Ok(Extent { read_pos: key.k.start_pos(), btree, k: key, offset: 0, sectors: range.len() >> 9 })
}

/// The extent read at @read_pos, by @k, the extents btree's slot there: its
/// key copied into the attempt's memory, as fragment_of() unlocks the
/// transaction before it's done with it.
fn extent_of<'a>(t: &TransAttempt<'a, '_>, read_pos: Bpos, k: BkeySC<'_>) -> Result<Extent<'a>, BchError> {
    let mut offset = (read_pos.offset - k.k.start_offset()) as i64;
    let sectors = k.k.size - offset as u32;

    let (k, btree) = match k.arm::<c::ReflinkP>() {
        Some(p) => {
            let mut it2 = BtreeIter::uninit();
            let ik = lookup_indirect_extent(t, &mut it2, &mut offset, p, true, BtreeIterFlags::empty())?;
            if ik.k.key_type().is_deleted() {
                return Err(t.fs().err(err::BCH_ERR_missing_indirect_extent));
            }
            (BkeySC::from(t.bkey_reassemble(ik)?), Btree::reflink)
        }
        None => (BkeySC::from(t.bkey_reassemble(k)?), Btree::extents),
    };
    let sectors = sectors.min(k.k.size - offset as u32);
    Ok(Extent { read_pos, btree, k, offset: offset as u32, sectors })
}

#[cold]
fn read_err_msg(trans: &BtreeTrans<'_>, out: &mut Printbuf, read: &Rbio, read_pos: Bpos) {
    write!(out, "data read error at ");
    bch2_inum_offset_err_msg_trans(trans, out, read.subvol, read_pos);
    if read.is_update() {
        write!(out, " (internal move) ");
    }
    write!(out, ": ");
}

/// read_err_msg(), inside @t's attempt: the inode's path is looked up in it,
/// so a restart there is the caller's, and the message is dropped.
#[cold]
fn read_err_msg_in_attempt(t: &TransAttempt<'_, '_>, out: &mut Printbuf, read: &Rbio, read_pos: Bpos)
    -> Result<(), BchError>
{
    write!(out, "data read error at ");
    if let Err(e) = inum_offset_err_msg(t, out, read.subvol, read_pos) {
        out.set_suppressed(true);
        return Err(e);
    }
    if read.is_update() {
        write!(out, " (internal move) ");
    }
    write!(out, ": ");
    Ok(())
}

fn is_csum_err(e: BchError) -> bool {
    e.matches(err::BCH_ERR_data_read_csum_err) ||
        e.matches(err::BCH_ERR_data_read_retry_csum_err) ||
        e.matches(err::BCH_ERR_data_read_retry_csum_err_maybe_userspace)
}

pub fn data_read_sb_err(e: BchError) -> SbErrorId {
    if e.matches(err::BCH_ERR_decompress) {
        bch2_decompress_sb_err(e.raw())
    } else if is_csum_err(e) {
        id::data_read_csum_err
    } else {
        id::data_read_io_err
    }
}

#[cold]
fn retry_report(read: &Rbio, read_pos: Bpos, k: BkeySC<'_>,
                failed: &c::bch_io_failures, orig_ret: Result<(), BchError>, ret: Result<(), BchError>) {
    let fs = read.fs();
    let trans = btree_trans!(fs);
    let trans = &trans;
    let ret = ret.err();
    let have_key = !k.k.key_type().is_deleted();

    if !failed.is_empty() || failed.ec_msg.pos != 0 || ret.is_some() {
        let msg = |out: &mut Printbuf| {
            read_err_msg(trans, out, read, read_pos);
            out.newline();
            if have_key {
                write!(out, "{}\n", k.to_text(fs));
            }
            failed.to_text(out, fs);
            match ret {
                None => {
                    write!(out, "successful retry");
                    if read.self_healing.load(Relaxed) {
                        write!(out, ", self healing");
                    }
                }
                Some(e) => write!(out, "error {e}"),
            }
        };

        if !read.report(failed.err_mask(), &msg) {
            let mut m = log_msg_ratelimited!(fs, ret.is_some() as usize, 2);
            msg(&mut m);
        }
    }

    if !failed.is_empty() || ret.is_some() {
        let outcome = match ret {
            None if orig_ret.is_err_and(is_csum_err) => id::data_read_csum_err_recovered,
            None                                     => id::data_read_io_err_recovered,
            Some(e) if is_csum_err(e)                => id::data_read_csum_err,
            Some(_)                                  => id::data_read_io_err,
        };
        fs.sb_error_count(outcome);

        // Only for failures that came back from the block layer: errcode also
        // holds our own verdicts (device offline, stale pointer, decompress
        // error), and those aren't block statuses to be counted as unknown
        // ones. Decompression failures are counted where they happen, in
        // bio_uncompress().
        failed.count_blk_errors(fs);

        let damage = ret.map_or(outcome, data_read_sb_err);
        if !read.is_update() && read.subvol != 0 && have_key {
            let _ = commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| damage::record(t, read_pos, damage));
        }
    }
}

fn should_retry(e: BchError) -> bool {
    e.matches(err::BCH_ERR_transaction_restart) ||
        e.matches(err::BCH_ERR_data_read_retry) ||
        e.matches(err::BCH_ERR_blockdev_io_error)
}

fn retry_state(mut f: Fragment<'_>, ret: Result<(), BchError>) -> RetryState<'_> {
    f.rbio().fs().event_inc_trace(events::data_read_retry, |out| f.to_text(out));
    let mut failures = FailureState { io: c::bch_io_failures::default(), read: f.plan.landed.then_some(f.plan.pick), poisoned: None };
    if let Err(e) = ret {
        failed_bio_to_text(&mut failures.io, f.bio(), e);
    }
    if !f.split() {
        f.bio_mut().set_status(0);
        f.rbio().set_ret(Ok(()));
    }
    let s = RetryState { plan: f.plan, ret, failures };
    f.free(ret);
    s
}

fn resume_target(f: &Fragment<'_>) -> Resume<'static> {
    let crc = &f.plan.pick.crc;
    let csum_type = c::CsumType(crc.csum_type.into());
    if f.plan.narrow_crcs || matches!(f.kind, Kind::Promote(..)) ||
              crc_is_compressed(crc) || csum_type.is_encryption() {
        Resume::queue(Queue::system_dfl())
    } else if crc.csum_type != 0 {
        Resume::queue(Queue::system_highpri())
    } else {
        Resume::INLINE
    }
}

fn can_narrow_crc(n: &c::bch_extent_crc_unpacked) -> bool {
    n.csum_type != 0 && n.live_size < n.uncompressed_size && !crc_is_compressed(n)
}

/// @read's read of @sectors of @k, at @read_pos, traced as @e.
fn trace_read<E: Event<Kind = Sectors>>(read: &Rbio, e: E, sectors: u64, k: BkeySC<'_>, read_pos: Bpos, flags: F) {
    let fs = read.fs();
    fs.event_add_trace(e, sectors, |out| {
        write!(out, "{}\n", k.to_text(fs));
        read.to_text_atomic(out, read_pos, flags);
    });
}

/// The options a read of @k goes by: an indirect extent's own, looked up into
/// @indirect, or the read's.
// Out of line, and into @indirect: a bch_inode_opts returned through
// fragment_of()'s frame is two of them there
#[inline(never)]
fn read_opts<'o>(t: &TransAttempt<'_, '_>, orig: &'o Rbio, k: BkeySC<'_>, indirect: &'o mut Option<c::bch_inode_opts>)
    -> Result<&'o c::bch_inode_opts, BchError>
{
    if k.arm::<Indirect>().is_none() {
        return Ok(&orig.opts);
    }
    Ok(indirect.insert(get_io_opts(t, None, k)?))
}

// Out of line: inlined, its btree update is in the frame of every read's
// fragment_of()
#[inline(never)]
fn read_inline(t: &TransAttempt<'_, '_>, read: &Rbio, mut r: BioRange<'_>, e: Extent<'_>, flags: F,
               in_retry: bool) -> Result<(), BchError> {
    let fs = t.fs();
    trace_read(read, events::data_read_inline, (r.len() >> 9).into(), e.k, e.read_pos, flags);

    let d = InlineDataRef::of(e.k).expect("inline data");
    if d.poisoned() && !flags.contains(F::no_poison_check) {
        return Err(fs.err(err::BCH_ERR_extent_poisoned));
    }
    if let Err(mismatch) = d.verify() {
        return Err(read_inline_csum_err(t, read, e, in_retry, mismatch));
    }

    // past the data, zeroes:
    let data = d.read_from((e.offset as usize) << 9);
    let n = r.copy_from(&data[..data.len().min(r.len() as usize)]);
    r.advance(n as u32);
    r.zero();
    Ok(())
}

/*
 * An inline data extent with its own checksum failed it: there's no other copy
 * to read, so the read fails. In the retry path, the retry reports it, and
 * records the damage.
 *
 * The key is poisoned, its damage recorded in the same transaction: once, by
 * whichever read's poisoning commits first - reads of it fail early after.
 */
#[cold]
fn read_inline_csum_err(t: &TransAttempt<'_, '_>, read: &Rbio, e: Extent<'_>, in_retry: bool,
                        mismatch: CsumMismatch) -> BchError {
    let fs = t.fs();
    let ret = fs.err(err::BCH_ERR_data_read_csum_err);

    // first: its lookup can restart, and what follows mustn't be done twice
    let msg = if in_retry {
        None
    } else {
        let mut m = log_msg_ratelimited!(fs);
        if let Err(restart) = read_err_msg_in_attempt(t, &mut m, read, e.read_pos) {
            return restart;
        }
        Some(m)
    };

    inline_data::poison(fs, e.btree, e.k, e.read_pos, !in_retry && !read.is_update());
    read.report(BCH_READ_ERR_checksum, |_| {});

    if let Some(mut m) = msg {
        fs.sb_error_count(id::data_read_csum_err);
        write!(m, "inline data ");
        mismatch.to_text(&mut m);
        write!(m, "\n  {}\n  error {ret}", e.k.to_text(fs));
    }
    ret
}

#[cold]
fn read_extent_pick_err(t: &TransAttempt<'_, '_>, orig: &Rbio, e: Extent<'_>, flags: F, ret: BchError,
                        failed: Option<&mut FailureState>) -> BchError {
    let fs = t.fs();
    let csum_err = ret == err::BCH_ERR_data_read_csum_err.into();
    match failed {
        Some(failed) if csum_err => maybe_poison_extent(t, orig, e, flags, failed).err().unwrap_or(ret),
        Some(_) => ret,
        None => {
            assert!(!csum_err, "a terminal checksum error outside a retry");
            let mut m = log_msg_ratelimited!(fs);
            if let Err(restart) = read_err_msg_in_attempt(t, &mut m, orig, e.read_pos) {
                return restart;
            }
            write!(m, "{ret}\n  {}", e.k.to_text(fs));
            ret
        }
    }
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
crate::module_param! {
    pub(crate) static read_corrupt_ratio: u32 = 0;
    perm = 0o644,
    desc = "";
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
crate::module_param! {
    pub(crate) static read_corrupt_device: i32 = 0;
    perm = 0o644,
    desc = "";
}

pub(crate) fn stale(fs: &Fs, flags: F) -> BchError {
    fs.err(if flags.contains(F::retry_if_stale) {
        err::BCH_ERR_data_read_ptr_stale_retry
    } else {
        err::BCH_ERR_data_read_ptr_stale_race
    })
}

fn narrow_crcs_from_read(fs: &Fs, rbio: &Rbio, p: &Plan<'_>) {
    let crc = p.pick.crc;
    if crc.csum_type == 0 || crc_is_compressed(&crc) {
        return;
    }

    let mut new = c::bch_extent_crc_unpacked::default();
    if fs.rechecksum_bio(rbio.bio(), p.version, crc, None, Some(&mut new), crc.offset.into(), crc.live_size,
                         crc.csum_type.into()).is_err() {
        bch_err!(fs, "error verifying existing checksum while narrowing checksum (memory corruption?)");
        return;
    }

    if !fs.flags.get(c::FsFlag::rw) {
        return;
    }

    let trans = btree_trans!(fs);
    let res = DiskReservation::new(fs);
    match commit_do(&trans, Some(&res), CommitFlags::NO_ENOSPC, |t| narrow_crcs_update(t, p, new)) {
        Ok(()) => fs.event_inc_trace(events::data_read_narrow_crcs, |out| rbio.to_text_atomic(out, p.read_pos, p.flags)),
        Err(e) => fs.event_inc_trace(events::data_read_narrow_crcs_fail, |out| {
            write!(out, "{e}\n");
            rbio.to_text_atomic(out, p.read_pos, p.flags);
        }),
    }
}

fn narrow_crcs_update(t: &TransAttempt<'_, '_>, p: &Plan<'_>, new: c::bch_extent_crc_unpacked) -> Result<(), BchError> {
    let fs = t.fs();
    let crc = p.pick.crc;
    let fail = || fs.err(err::BCH_ERR_rbio_narrow_crcs_fail);

    let mut iter = BtreeIter::new(t, p.data_btree, p.data_pos, BtreeIterFlags::INTENT);
    let Some(k) = iter.peek_slot(t)? else { return Err(fail()) };

    // wraps for an extent that starts below its crc's offset - one collapse
    // range moved down - as matches_ptr()'s arithmetic does
    let data_offset = p.data_pos.offset.wrapping_sub(crc.offset.into());
    if k.k.bversion != p.version ||
       !k.arm::<Ptrs>().is_some_and(|kp| kp.matches_ptr(fs, &p.pick.ptr, data_offset)) ||
       k.k.start_pos() != p.data_pos ||
       k.k.p.offset != p.data_pos.offset + u64::from(crc.live_size) {
        return Err(fail());
    }

    let mut n = t.bkey_reassemble_room(k, usize::from(k.k.u64s) * size_of::<u64>() +
                                           size_of::<c::bch_extent_crc128>())?;
    let narrowed = n.reborrow().arm::<PtrsMut>().expect("an extent with pointers, matched")
        .narrow_crc(fs, crc, new);
    if !narrowed {
        return Err(fail());
    }
    t.update(&iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

fn mark_failure(pick: &c::extent_ptr_decoded, failed: &mut c::bch_io_failures, e: BchError) -> BchError {
    if e.matches(err::BCH_ERR_data_read_retry_avoid) || e.matches(err::BCH_ERR_blockdev_io_error) {
        failed.mark(pick, e);
    }
    e
}

fn ptrs_io_error(fs: &Fs, k: BkeySC<'_>, failed: &c::bch_io_failures) -> u16 {
    let failed_dev = |dev: u32| failed.dev(dev).is_some_and(|f| f.errcode != 0 || f.csum_nr != 0);
    k.arm::<Ptrs>().map_or(0, |kp| {
        kp.ptrs(fs).enumerate().filter(|(_, p)| failed_dev(p.dev().into())).fold(0, |m, (i, _)| m | 1 << i)
    })
}

fn failed_bio_to_text(failed: &mut c::bch_io_failures, bio: &Bio, e: BchError) {
    if e == err::BCH_ERR_BLK_STS_INVAL.into() {
        let msg = &mut failed.ec_msg;
        write!(msg, "Failing bio, after block layer completion:\n");
        bio.to_text(&mut msg.indent(2));
    }
}

#[cold]
fn stale_dirty_pointer(trans: &BtreeTrans<'_>, ca: &Dev, k: BkeySC<'_>, ptr: c::bch_extent_ptr) {
    let fs = trans.fs();
    let bucket = ca.ptr_bucket_pos(&ptr);
    let mut buf = Printbuf::new();

    match ca.bucket_gen(bucket.offset, &read_lock()) {
        Some(generation) => {
            write!(buf, "Attempting to read from stale dirty pointer:\n");
            let mut out = buf.indent(2);
            write!(out, "{}\nmemory gen: {generation}", k.to_text(fs));

            let alloc = lockrestart_do(trans, |t| {
                let mut iter = BtreeIter::new(t, Btree::alloc, bucket, BtreeIterFlags::CACHED);
                Ok(iter.peek_slot(t)?.map(BkeyBuf::from_key))
            });
            if let Ok(Some(a)) = alloc {
                write!(out, "\n{}", BkeySC::from(&a).to_text(fs));
            }
        }
        None => {
            write!(buf, "Attempting to read from invalid bucket {}:{}:\n", { bucket.inode }, { bucket.offset });
            let mut out = buf.indent(2);
            let mi = ca.mi.get();
            write!(out, "first bucket {} nbuckets {}\n{}\n", { mi.first_bucket }, { mi.nbuckets },
                   k.to_text(fs));
        }
    }

    fs_inconsistent!(fs, "{buf}");
}

crate::module_param! {
    pub(crate) static poison_extents_on_checksum_error: bool = true;
    perm = 0o644,
    desc = "Extents with checksum errors are marked as poisoned - unsafe without read fua support";
}

fn maybe_poison_extent(t: &TransAttempt<'_, '_>, read: &Rbio, e: Extent<'_>, flags: F, failed: &mut FailureState)
    -> Result<(), BchError>
{
    let fs = t.fs();
    if !poison_extents_on_checksum_error.get() || !fs.flags.get(c::FsFlag::rw) {
        return Ok(());
    }

    let extent_flags = e.k.arm::<Ptrs>().map_or(c::ExtentFlags::empty(), |k| k.extent_flags(fs));
    if extent_flags.contains(c::ExtentFlags::poisoned) {
        return Ok(());
    }

    let mut iter = BtreeIter::new(t, e.btree, e.k.k.start_pos(), BtreeIterFlags::INTENT);
    let Some(k) = iter.peek_slot(t)? else { return Ok(()) };
    if !k.and_val_eq(e.k) {
        return Ok(());
    }
    let old = BkeyBuf::from_key(k);

    fs.request_incompat_feature(MetadataVersion::extent_flags)?;
    let mut new = t.bkey_reassemble_room(k, usize::from(k.k.u64s) * size_of::<u64>() +
                                             size_of::<c::bch_extent_flags>())?;
    new.reborrow().arm::<PtrsMut>().expect("poisoning an extent whose entries can't be edited")
        .extent_flags_set(fs, extent_flags | c::ExtentFlags::poisoned).expect("room for an extent flags entry: made for it");
    t.update(&iter, &new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    t.commit(Some(&DiskReservation::new(fs)), CommitFlags::empty())?;

    if read.is_update() {
        failed.poisoned = Some(BkeyBuf::from_key(new.as_sc()));
    }

    fs.event_inc_trace(events::data_read_fail_and_poison, |out| {
        write!(out, "{}\n", BkeySC::from(&old).to_text(fs));
        read.to_text_atomic(out, e.read_pos, flags);
    });
    Ok(())
}

fn fragment_of<'r, 'k>(t: &TransAttempt<'_, '_>, orig: &'r Rbio, mut range: BioRange<'r>, e: Extent<'k>,
                       mut failed: Option<&mut FailureState>, flags: F, split: bool, dev: u32)
    -> Result<Option<Planned<'r, 'k>>, BchError>
{
    assert!(range.is_of(orig.bio()), "a read into another bio's range");
    let Extent { read_pos, k, offset: mut offset_into_extent, .. } = e;
    let trans = t.trans();
    let fs = trans.fs();

    if k.k.extent_is_inline_data() {
        return read_inline(t, orig, range, e, flags, failed.is_some()).map(|()| None);
    }

    if k.arm::<Ptrs>().is_some_and(|kp| kp.poisoned(fs)) && !orig.is_update() {
        if !flags.contains(F::no_poison_check) {
            return Err(fs.err(err::BCH_ERR_extent_poisoned));
        }
        orig.report(BCH_READ_ERR_checksum, |_| {});
    }

    let mut pick = match k.pick_read_device(fs, failed.as_deref().map(|f| &f.io), dev, flags) {
        Ok(Some(p)) => p,
        Ok(None) => {
            trace_read(orig, events::data_read_hole, (range.len() >> 9).into(), k, read_pos, flags);
            range.zero();
            if orig.is_update() {
                return Err(fs.err(err::BCH_ERR_data_read_key_overwritten));
            }
            return Ok(None);
        }
        Err(ret) => return Err(read_extent_pick_err(t, orig, e, flags, ret, failed)),
    };

    let csum_type = c::CsumType(pick.crc.csum_type.into());
    if csum_type.is_encryption() && fs.encryption_key().is_none() {
        let mut m = log_msg_ratelimited!(fs);
        read_err_msg_in_attempt(t, &mut m, orig, read_pos)?;
        write!(m, "attempting to read encrypted data without encryption key\n  {}", k.to_text(fs));
        return Err(fs.err(err::BCH_ERR_data_read_no_encryption_key));
    }

    let ca: Option<DevIoRef<c::DevReadRefKind>> =
        if !pick.do_ec_reconstruct { fs.dev_get_ioref(pick.ptr.dev().into()) } else { None };

    // Stale dirty pointers are treated as IO errors, but @failed isn't
    // allocated unless we're in the retry path - so if we're not in the retry
    // path, don't check here, it'll be caught in fragment_io() and we'll end up
    // in the retry path:
    if let (Some(f), Some(ca)) = (failed.as_deref_mut(), &ca) {
        if pick.ptr.cached() == 0 && ca.dev_ptr_stale(&pick.ptr, &read_lock()) {
            stale_dirty_pointer(trans, ca, k, pick.ptr);
            return Err(mark_failure(&pick, &mut f.io, fs.err(err::BCH_ERR_data_read_ptr_stale_dirty)));
        }
    }

    let narrow_crcs = !orig.is_update() && failed.is_none() && can_narrow_crc(&pick.crc);
    let mut read_full = narrow_crcs;
    let mut bounce = narrow_crcs;

    if !orig.is_update() {
        debug_assert!(offset_into_extent + (range.len() >> 9) <= k.k.size,
                      "reading {} sectors at {offset_into_extent} of an extent of {}", range.len() >> 9, k.k.size);
    } else {
        // For data updates, iter.bi_size can be bigger than the key - it's just
        // the buffer size, we're not trying to read a specific amount of data
        // from this extent:
        //
        // If it's smaller, we raced with a merge during a retry:
        if pick.crc.uncompressed_size << 9 > range.len() {
            return Err(fs.err(err::BCH_ERR_data_read_buffer_too_small));
        }
        range = range.take(pick.crc.compressed_size << 9);
        read_full = true;
    }

    if orig.decodes() && (crc_is_compressed(&pick.crc) ||
                  (pick.crc.csum_type != 0 &&
                   ((range.len() >> 9) != pick.crc.uncompressed_size ||
                    (csum_type.is_encryption() && flags.contains(F::user_mapped)) ||
                    flags.contains(F::must_bounce)))) {
        read_full = true;
        bounce = true;
    }

    let mut data_pos = k.k.start_pos();

    // Shared data has options of its own, and its readers go by them: the
    // promote's index update judges the copy by the extent's options, so a
    // copy made by the reader's would be dropped in the commit that adds it,
    // and written again on every read.
    //
    // TODO: carry the compact reconcile opts through the read and write paths
    // instead of a full bch_inode_opts; then the read bio can hold the
    // effective opts per extent, and this goes.
    let mut indirect_opts = None;
    let opts = read_opts(t, orig, k, &mut indirect_opts)?;

    // If it's being moved internally, we don't want to flag it as a cache
    // hit:
    if let Some(ca) = &ca {
        if pick.ptr.cached() != 0 && !orig.is_update() {
            match bucket_read_time_reset(t, pos(pick.ptr.dev().into(), ca.ptr_bucket(&pick.ptr))) {
                Err(e) if is_restart(&e) => return Err(e),
                _ => {}
            }
        }
    }

    let have_io_error = failed.as_deref().is_some_and(|f| !f.io.is_empty());
    let have_failures = failed.is_some();
    // No promote for a reconstruct: it's punted to the retry before anything
    // is read, and the promote would be started there with nothing in its
    // bounce buffer. An error is a transaction restart, from promote setup;
    // propagated before we unlock, so the read retries.
    let promote = if !orig.is_update() &&
                     ((opts.promote_target != 0 && flags.contains(F::may_promote) && !pick.do_ec_reconstruct) ||
                      have_io_error) {
        promote_alloc(t, k, orig, opts, failed.map(|f| &mut f.io))?
    } else {
        None
    };
    if let Some(p) = &promote {
        bounce = true;
        read_full |= p.whole;
    }

    // Done with btree operations: unlock the iterator while the btree node's
    // lock is still in cache, before allocating the clone/fragment (if any)
    // and doing the IO:
    t.unlock();

    if !read_full {
        data_pos.offset += u64::from(offset_into_extent);
        pick.ptr.set_offset(pick.ptr.offset() + u64::from(pick.crc.offset) + u64::from(offset_into_extent));
        offset_into_extent = 0;
        let sectors = range.len() >> 9;
        pick.crc.compressed_size = sectors;
        pick.crc.uncompressed_size = sectors;
        pick.crc.offset = 0;
        pick.crc.live_size = sectors;
    }

    let plan = Plan { range, flags, pick, read_pos, data_btree: e.btree, data_pos, version: k.k.bversion,
                      offset_into_extent, narrow_crcs, landed: false };

    let how = if let Some(p) = promote {
        How::Promote(p)
    } else if bounce {
        // Only skip zeroing if we can detect if the device lied and didn't
        // DMA.
        //
        // If @failed is set, we might be in the extended read call that allows
        // returning data to userspace that didn't pass the checksum check, so
        // userspace can get back mangled data to attempt its own recovery:
        let mut gfp = c::GFP_NOIO;
        if pick.crc.csum_type != 0 && !have_failures {
            gfp |= c::__GFP_SKIP_ZERO;
        }
        How::Bounce((pick.crc.compressed_size << 9) as usize, gfp)
    } else if split {
        // Have to clone if there were any splits, due to error reporting issues
        // (if a split errored, and retrying didn't work, when it reports the
        // error to its parent (us) we don't know if the error was from our bio,
        // and we should retry, or from the whole bio, in which case we don't
        // want to retry and lose the error)
        How::Clone
    } else {
        How::Own
    };
    Ok(Some(Planned { how, plan, ca, k }))
}

/// What a fragment's bio is to be.
enum How<'r> {
    /// The read's own: not split
    Own,
    Clone,
    /// Pages of its own: the size, and how to allocate them
    Bounce(usize, c::gfp_t),
    Promote(Promoted<'r>),
}

/// A fragment as fragment_of() picked it, its bio not yet made: made into one
/// its caller reads by make(), or started as a task of its own by start().
struct Planned<'r, 'k> {
    how:  How<'r>,
    plan: Plan<'r>,
    ca:   Option<DevIoRef<c::DevReadRefKind>>,
    k:    BkeySC<'k>,
}

impl<'r> Planned<'r, '_> {
    /// The fragment, its bio of the filesystem's bioset, made into @into: for a
    /// read awaited in its caller's task - a retry's, a data update's.
    fn make(self, orig: &'r Rbio, into: &mut Option<Fragment<'r>>) -> Result<(), BchError> {
        let Planned { how, plan, ca, k } = self;
        let fs = orig.fs();
        let opf = orig.bio().raw().bi_opf;

        let _noio = MemallocFlags::noio();
        let set = &fs.bio_read_split;
        let fragment = |b| Rbio::fragment(orig, b);
        let kind = match how {
            How::Promote(p) => Kind::Promote(p.rbio.bio.lend(), p.task),
            How::Bounce(size, gfp) => {
                let bs = fs.opts().block_size.get().into();
                Kind::Bounce(set.alloc_with(core::ptr::null_mut(), 0, (&fs.bio_bounce, bs, size, gfp), fragment)?)
            }
            How::Clone => Kind::Clone(set.alloc_with(core::ptr::null_mut(), 0, plan.range, fragment)?),
            How::Own => Kind::Own(orig.lend()),
        };

        let f = into.insert(Fragment::new(fs, kind, plan));
        match f.kind {
            // promote already allocated bounce rbio: promote needs to allocate a
            // bio big enough for uncompressing data in the write path, but we're
            // not going to use it all here:
            Kind::Promote(..) => f.bio_mut().shrink(plan.pick.crc.compressed_size << 9),
            Kind::Own(_) => {
                f.plan.landed = true;
                f.bio_mut().set_range(plan.range);
                f.rbio().set_ret(Ok(()));
            }
            _ => {}
        }

        f.ca = ca;
        f.bio_mut().set_opf(opf);
        f.bio_mut().set_sector(plan.pick.ptr.offset());
        trace_fragment(orig, f.rbio(), &plan, f.bounced(), k);
        Ok(())
    }

    /// The fragment started: a task in a box of its own, from the module's
    /// pool, which puts what it read to @reads - or, for the read's own bio,
    /// the walk's last, made into @own for read() to read.
    fn start(self, t: &TransAttempt<'_, '_>, orig: &'r Rbio, reads: &Reads, split_trace: bool,
             own: &mut Option<Fragment<'r>>) -> Result<(), BchError>
    {
        match self.how {
            How::Own => return self.make(orig, own),
            How::Promote(_) => return self.start_promote(t, orig, reads, split_trace),
            How::Clone | How::Bounce(..) => {}
        }

        let fs = orig.fs();
        let Planned { how, plan, ca, k } = self;
        let bounce = match how {
            How::Clone => None,
            How::Bounce(size, gfp) => Some((&fs.bio_bounce, fs.opts().block_size.get().into(), size, gfp)),
            How::Own | How::Promote(_) => unreachable!("a fragment start() made or started above"),
        };
        let nr_vecs = bounce.as_ref().map_or(0, BioBuffersOwned::nr_vecs);
        let opf = orig.bio().raw().bi_opf;

        let _noio = MemallocFlags::noio();
        let mut slot = TaskSlot::exclusive_bio_in(&FRAGMENTS, core::ptr::null_mut(), nr_vecs, 0, |b| {
            pin_init!(FragmentBio { rbio <- Rbio::fragment(orig, b) }? BchError)
        })?;
        let buffers = match bounce {
            Some(src) => Some(BioBuffersOwned::fill(slot.home().bio_mut(), src)?),
            None => {
                BioClone::fill(slot.home().bio_mut(), plan.range)?;
                None
            }
        };
        let mut bio = slot.home().bio_mut();
        bio.as_mut().set_opf(opf);
        bio.set_sector(plan.pick.ptr.offset());
        let rb = &slot.home().into_ref().get_ref().rbio;
        trace_fragment(orig, rb, &plan, bounce.is_some(), k);
        if split_trace {
            fs.event_inc_trace(events::data_read_split, |out| rb.to_text_atomic(out, plan.read_pos, plan.flags));
        }

        let r = reads.get();
        let make = move |home| fragment_task(r, Fragment { ca, ..Fragment::new(orig.fs(), Kind::Home(home, buffers), plan) },
                                             orig);
        start_task(t, plan.pick.do_ec_reconstruct, |inline| unsafe {
            if inline { slot.start_inline(Queue::system_dfl(), make) } else { slot.start(Queue::system_dfl(), make) }
        });
        Ok(())
    }

    /// start(), for a promote's fragment: its bio is the promote's, and the
    /// task in a box from the module's pool.
    // Apart: the fragment, made here and moved into its task, isn't in the
    // frame of every read's start()
    #[inline(never)]
    fn start_promote(self, t: &TransAttempt<'_, '_>, orig: &'r Rbio, reads: &Reads, split_trace: bool)
        -> Result<(), BchError>
    {
        let fs = orig.fs();
        let reconstruct = self.plan.pick.do_ec_reconstruct;
        let mut f = None;
        self.make(orig, &mut f)?;
        let f = f.expect("the promote's fragment, just made");
        if split_trace {
            fs.event_inc_trace(events::data_read_split, |out| f.to_text(out));
        }

        let _noio = MemallocFlags::noio();
        let slot = match TaskSlot::exclusive_in(&FRAGMENTS, ()) {
            Ok(slot) => slot,
            // the promote ended with the error, not dropped unstarted
            Err(e) => {
                f.free(Err(e));
                return Err(e);
            }
        };
        let r = reads.get();
        let make = move |_| fragment_task(r, f, orig);
        start_task(t, reconstruct, |inline| unsafe {
            if inline { slot.start_inline(Queue::system_dfl(), make) } else { slot.start(Queue::system_dfl(), make) }
        });
        Ok(())
    }
}

/// A fragment's read of @plan, into @rb, traced and counted.
fn trace_fragment(orig: &Rbio, rb: &Rbio, plan: &Plan<'_>, bounced: bool, k: BkeySC<'_>) {
    let fs = orig.fs();
    let Plan { read_pos, flags, .. } = *plan;
    if bounced {
        fs.event_inc_trace(events::data_read_bounce, |out| rb.to_text_atomic(out, read_pos, flags));
    }
    let sectors = (rb.bio().raw().bi_iter.bi_size >> 9).into();
    if orig.is_update() {
        trace_read(rb, events::data_update_read, sectors, k, read_pos, flags);
    } else {
        trace_read(rb, events::data_read, sectors, k, read_pos, flags);
    }

    fs.io_clock[READ].increment(sectors);
}

/// The futures made in boxes of their own, apart: before rustc 1.87, every
/// item of the module a future's type is named in that has the type in its
/// signature must be its defining use - here, only the fns that make them.
mod boxed {
    use super::*;

    /// @f's read of its stripe, its own task joined: in a box, made when it's
    /// awaited, so that a fragment's future isn't the size of one.
    #[in_place(EcReadFut)]
    pub(super) async fn ec_read<'f, 'r: 'f>(f: &'f mut Fragment<'r>, fs: &'f Fs, msg: &'f mut Printbuf)
        -> Result<(), BchError>
    {
        let waiter = waiter::Async::current().await;
        let (bio, p) = (f.kind.bio_mut(), &f.plan);
        let mut join = Join::new();
        let wait = {
            let _noio = MemallocFlags::noio();
            join.spawn(Queue::system_dfl(),
                       move || ec::io::read_extent(fs, p.data_btree, p.data_pos, bio, &p.pick, p.flags, msg), waiter)
        }.map_err(|_| fs.err(err::BCH_ERR_ENOMEM_ec_read_extent))?;
        wait.resume_on(Resume::SPAWNED).await.expect("a joined task's output, handed back")
    }

    /// A retry, its state and its future together in an element of the
    /// filesystem's retry pool: a fragment's future holds neither.
    #[in_place(RetryOwnedFut)]
    pub(super) async fn retry_owned<'a>(read: &'a Rbio, mut s: RetryState<'a>, at: Locate<'a>)
        -> (Result<(), BchError>, FailureState)
    {
        (retry(read, &mut s, at).await, s.failures)
    }
}
use boxed::{ec_read, retry_owned, RetryOwnedFut};

fn device_read_start(f: &mut Fragment<'_>, fs: &Fs) -> Result<u64, BchError> {
    let Some(ca) = f.ca.as_deref() else { return Err(fs.err(err::BCH_ERR_data_read_retry_device_offline)) };
    let bdev = ca.disk_sb.bdev;
    ca.io_done_add(READ, DataType::user, (f.bio().raw().bi_iter.bi_size >> 9).into());
    f.bio_mut().set_dev(bdev);
    Ok(local_clock())
}

fn device_read_done(f: &Fragment<'_>, fs: &Fs, submit_time: u64, status: Result<(), BchError>)
    -> Result<(), BchError>
{
    if let Some(ca) = f.ca.as_deref() {
        ca.account_io_completion(c::MemberErrorType::read, submit_time, status.is_ok());
    }
    status.map_err(|e| fs.err(e))
}

#[args_stored_once]
async fn fragment_io(f: &mut Fragment<'_>, read: &Rbio, target: Resume<'static>, ec_msg: Option<&mut Printbuf>)
    -> Result<(), BchError>
{
    let fs = read.fs();
    if f.plan.pick.do_ec_reconstruct {
        let msg = ec_msg.expect("a reconstruct, in a retry");
        let f = &mut *f;
        let init = unsafe {
            ::pin_init::init_from_closure(move |slot| {
                core::ptr::write(slot, ec_read(f, fs, msg));
                Ok::<_, AllocError>(())
            })
        };
        let mut ec = {
            let _noio = MemallocFlags::noio();
            AllocBox::new(Kmalloc, init)
        }.map_err(|_| fs.err(err::BCH_ERR_ENOMEM_ec_read_extent))?;
        // the box's contents never move: only the box does
        unsafe { Pin::new_unchecked(&mut *ec) }.await?;
    } else {
        let submit_time = device_read_start(f, fs)?;
        let status = if fs.opts().no_data_io.get() != 0 {
            Ok(())
        } else {
            f.submit(waiter::Async::current().await).resume_on(Resume::INLINE).await
        };
        let ret = device_read_done(f, fs, submit_time, status);
        if let ResumeAt::Queue(q) = target.get() {
            requeue(q).await;
        }
        ret?;
    }
    fragment_read_done(f, read)
}

/// What @f read, checked - and copied back from a bounce, decrypted,
/// decompressed - into the read's bio.
fn fragment_read_done(f: &mut Fragment<'_>, read: &Rbio) -> Result<(), BchError> {
    let fs = read.fs();
    let no_data_io = fs.opts().no_data_io.get() != 0;
    let (bounced, split, promote) = (f.bounced(), f.split(), matches!(f.kind, Kind::Promote(..)));
    let iter = if bounced {
        let mut iter = f.bio().raw().bi_iter;
        iter.bi_size = f.plan.pick.crc.compressed_size << 9;
        iter.bi_idx = 0;
        reset_bvec_offset(&mut iter);
        iter
    } else {
        f.plan.range.pos()
    };
    unsafe { f.bio_mut().set_iter(iter) };

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    {
        let dev = read_corrupt_device.get();
        let ratio = read_corrupt_ratio.get();
        if ratio != 0 && (dev < 0 || dev == f.plan.pick.ptr.dev() as i32) &&
           crate::os::kernel::random_u64_below(ratio.into()) == 0 {
            f.bio_mut().corrupt();
        }
    }

    let (r, ca, p) = (f.kind.rbio(), f.ca.as_deref().map(|ca| &**ca), &mut f.plan);
    if ca.is_some_and(|ca| ca.dev_ptr_stale(&p.pick.ptr, &read_lock())) {
        fs.event_inc_trace(events::data_read_reuse_race, |out| {
            let _noreclaim = MemallocFlags::noreclaim();
            r.to_text_atomic(out, p.read_pos, p.flags)
        });
        return Err(stale(fs, p.flags));
    }

    let _noio = MemallocFlags::noio();
    let mut src = r.bio().range();
    let dst = p.range;
    let mut crc = p.pick.crc;
    let mut nonce = extent_nonce(p.version, &crc);
    let csum_type = c::CsumType(crc.csum_type.into());

    let csum = fs.checksum_range(csum_type, nonce, src);
    let csum_good = csum == p.pick.crc.csum || no_data_io;

    // Checksum error: if the bio wasn't bounced, we may have been reading
    // into buffers owned by userspace (that userspace can scribble over) -
    // retry the read, bouncing it this time:
    if !csum_good && !bounced && p.flags.contains(F::user_mapped) {
        p.flags |= F::must_bounce;
        return Err(fs.err(err::BCH_ERR_data_read_retry_csum_err_maybe_userspace));
    }

    if let Some(ca) = ca {
        ca.account_io_completion(c::MemberErrorType::checksum, 0, csum_good);
    }

    // XXX
    // We need to rework the narrow_crcs path to deliver the read completion
    // first, and then punt to a different workqueue, otherwise we're holding
    // up reads while doing btree updates which is bad for memory reclaim.
    if p.narrow_crcs && csum_good {
        narrow_crcs_from_read(fs, r, p);
    }

    let decrypt = |src: BioRange<'_>, crc: &c::bch_extent_crc_unpacked, nonce: Nonce| -> Result<(), BchError> {
        if c::CsumType(crc.csum_type.into()).is_encryption() {
            fs.encrypt_range(nonce, src).map_err(|_| fs.err(err::BCH_ERR_data_read_decrypt_err))
        } else {
            Ok(())
        }
    };
    let decompress = |src: BioRange<'_>, dst: BioRange<'_>, crc: &c::bch_extent_crc_unpacked, nonce: Nonce|
        -> Result<(), BchError>
    {
        assert!(bounced, "a compressed read not bounced");
        decrypt(src, crc, nonce)?;
        let ret = bio_uncompress(fs, src, dst, *crc);
        // On a checksum error we still try to decompress - a bitflip often
        // decompresses fine, and we return what we have. But the checksum
        // error is the one that describes what happened, so don't report a
        // decompression failure over it: fall through to the !csum_good check
        // below.
        if !no_data_io && csum_good { ret } else { Ok(()) }
    };

    if !read.is_update() {
        crc.offset += p.offset_into_extent as u16;
        crc.live_size = p.range.len() >> 9;

        if crc_is_compressed(&crc) {
            decompress(src, dst, &crc, nonce)?;
        } else {
            nonce = nonce + (u32::from(crc.offset) << 9);
            src.advance(u32::from(crc.offset) << 9);
            assert!(src.len() >= dst.len(), "a read bio short of its destination");
            src = src.take(dst.len());
            decrypt(src, &crc, nonce)?;
            if bounced {
                dst.copy_from_range(src);
            }
        }
    } else {
        if read.decodes() && crc_is_compressed(&crc) {
            // dst_iter doesn't make sense here, it refers to the size of the
            // compressed extent on disk (what the data update path generally
            // wants), and here we're just verifying that the data
            // decompresses and throwing away the result:
            decompress(src, dst.take(0), &crc, nonce)?;
            // We decrypted to decompress; re-encrypt:
            decrypt(src, &crc, nonce)?;
        }
        if split {
            p.landed = true;
        }
        if bounced {
            dst.copy_from_range(src);
        }
    }

    if !csum_good {
        return Err(fs.err(err::BCH_ERR_data_read_retry_csum_err));
    }

    // Re encrypt data we decrypted, so it's consistent with its crc:
    if promote {
        decrypt(src, &crc, nonce)?;
    }
    Ok(())
}

/// The retry's next fragment, made into @f, and the walk's step past it.
#[inline(never)]
fn retry_fragment<'r, P: WalkPos>(read: &'r Rbio, at: Locate<'_>, walk: &mut Walk<'r, P>,
                                  failed: &mut FailureState, prev_read: &mut BkeyBuf, f: &mut Option<Fragment<'r>>)
    -> Result<Step, BchError>
{
    let trans = btree_trans!(read.fs());
    lockrestart_do(&trans, |t| {
        let e = locate(t, at, walk.range())?;
        retry_fragment_of(t, read, e, walk, failed, prev_read, f)
    })
}

/// retry_fragment()'s fragment of @e, which locate() found.
// Apart: what it makes isn't in the frame locate()'s walk of the btree runs
// under
#[inline(never)]
fn retry_fragment_of<'r, P: WalkPos>(t: &TransAttempt<'_, '_>, read: &'r Rbio, e: Extent<'_>, walk: &mut Walk<'r, P>,
                                     failed: &mut FailureState, prev_read: &mut BkeyBuf, f: &mut Option<Fragment<'r>>)
    -> Result<Step, BchError>
{
    if !e.k.and_val_eq(BkeySC::from(&*prev_read)) {
        failed.io.reset_devs();
        prev_read.reassemble(e.k);
    }
    let (range, step) = walk.fragment(&e);
    if let Some(p) = fragment_of(t, read, range, e, Some(&mut *failed), walk.flags, walk.split, u32::MAX)? {
        p.make(read, f)?;
    }
    Ok(step)
}

#[args_stored_once]
async fn retry_walk(read: &Rbio, range: BioRange<'_>, at: Locate<'_>,
                    failed: &mut FailureState, prev_read: &mut BkeyBuf, flags: F) -> Result<(), BchError> {
    assert!(range.is_of(read.bio()), "a retry of another bio's range");
    let mut walk = Walk { read, pos: range.pos(), flags, split: true };
    let ret = loop {
        let mut f = None;
        let step = retry_fragment(read, at, &mut walk, failed, prev_read, &mut f);

        let step = match (step, &mut f) {
            (Ok(step), Some(frag)) => {
                let ret = fragment_io(frag, read, Resume::queue(Queue::system_dfl()), Some(&mut failed.io.ec_msg)).await;
                let f = f.take().expect("the fragment just read");
                f.end_in_retry(read, failed, ret).map(|()| step)
            }
            (step, _) => step,
        };
        if let ControlFlow::Break(ret) = walk.step(step) {
            break ret;
        }
    };

    if ret.is_err() {
        read.set_ret(ret);
    }
    ret
}

/// The key at @data_pos, into @sk, if it's still the one @pick was read from.
// Out of line, as are the other steps of the read's futures: a step's
// transaction and iterator, inlined, are in the poll's frame - under every
// await, and whatever the await runs on this stack. And a step makes its
// Fragment into the caller's Option: one returned, or moved through the poll,
// is a copy of it in the frame.
#[inline(never)]
fn retry_key(fs: &Fs, data_btree: Btree, data_pos: Bpos, pick: &c::extent_ptr_decoded, sk: &mut BkeyBuf) {
    // @pick may have been moved to the part of the extent read, with
    // @data_pos: matched by where its data starts
    let data_offset = data_pos.offset.wrapping_sub(pick.crc.offset.into());
    let trans = btree_trans!(fs);
    let _ = lockrestart_do(&trans, |t| {
        let mut iter = BtreeIter::new(t, data_btree, data_pos, BtreeIterFlags::empty());
        if let Some(k) = iter.peek_slot(t)? {
            if k.arm::<Ptrs>().is_some_and(|kp| kp.matches_ptr(fs, &pick.ptr, data_offset)) {
                sk.reassemble(k);
            }
        }
        Ok(())
    });
}

#[args_stored_once]
async fn retry(read: &Rbio, s: &mut RetryState<'_>, at: Locate<'_>) -> Result<(), BchError> {
    let fs = read.fs();
    let RetryState { plan: Plan { range, flags, read_pos, data_btree, data_pos, pick, .. }, ret: orig_ret,
                     ref mut failures, .. } = *s;
    let mut sk = BkeyBuf::new();
    retry_key(fs, data_btree, data_pos, &pick, &mut sk);

    if let Err(e) = orig_ret {
        if !BkeySC::from(&sk).k.key_type().is_deleted() {
            mark_failure(&pick, &mut failures.io, e);
        }
    }

    let flags = flags - (F::hard_require_read_device | F::may_promote);

    let ret = retry_walk(read, range, at, failures, &mut sk, flags).await;

    let reported = if read.is_update() && ret.is_err_and(read_err_benign) { Ok(()) } else { ret };
    retry_report(read, read_pos, BkeySC::from(&sk), &failures.io, orig_ret, reported);
    ret
}

struct RetryBox<'a>(AllocBox<RetryOwnedFut<'a>, &'a Mempool<Kmalloc>>);

impl Future for RetryBox<'_> {
    type Output = (Result<(), BchError>, FailureState);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // the box's contents never move: only the box does
        unsafe { Pin::new_unchecked(&mut *self.get_mut().0) }.poll(cx)
    }
}

/// @f's retry - @range, if given, for its plan's - made in its element, to be
/// awaited.
// Out of line, @f ended in it: its RetryState is in this frame, not its
// caller's
#[inline(never)]
fn retry_boxed<'a>(read: &'a Rbio, f: Fragment<'a>, ret: Result<(), BchError>, at: Locate<'a>,
                   range: Option<BioRange<'a>>) -> RetryBox<'a> {
    // ended - its bio and its device ref given back - before the wait on the
    // pool: not held while waiting on another mempool
    let mut s = retry_state(f, ret);
    if let Some(range) = range {
        s.plan.range = range;
    }
    let init = unsafe {
        ::pin_init::init_from_closure(move |slot| {
            core::ptr::write(slot, retry_owned(read, s, at));
            Ok::<_, AllocError>(())
        })
    };
    let _noio = MemallocFlags::noio();
    RetryBox(AllocBox::new(&read.fs().read_retry_pool, init).expect("a retry's slot, outside NORECLAIM"))
}

/// A fragment's task: @f read - and retried, if that's what it needs - what's
/// the filesystem's given back by the fragment's end, and what it read put to
/// @r last, as the read and the filesystem may go once it is. In a box of its
/// own, or awaited by read() for the read's own bio.
#[in_place(FragmentFut)]
async fn fragment_task<'r>(r: ReadRef, mut f: Fragment<'r>, read: &'r Rbio) {
    let ret = if f.plan.pick.do_ec_reconstruct {
        Ok(())
    } else {
        // fragment_io(), in this future: a future it awaited would hold its
        // own copies of what's here
        let ret = match device_read_start(&mut f, read.fs()) {
            Err(e) => Err(e),
            Ok(submit_time) => {
                let status = if read.fs().opts().no_data_io.get() != 0 {
                    Ok(())
                } else {
                    f.submit(waiter::Async::current().await).resume_on(Resume::INLINE).await
                };
                let ret = device_read_done(&f, read.fs(), submit_time, status);
                if let ResumeAt::Queue(q) = resume_target(&f).get() {
                    requeue(q).await;
                }
                ret.and_then(|()| fragment_read_done(&mut f, read))
            }
        };
        if let Err(e) = ret {
            read.saw_error.store(true, Relaxed);
            if should_retry(e) {
                requeue(Queue::system_dfl()).await;
            }
        }
        ret
    };
    // the retry's state goes in its box, before the await: not in this future
    let inum = c::subvol_inum { subvol: read.subvol.into(), inum: f.plan.read_pos.inode };
    let retry = match f.end(read, ret, Locate::Inode(inum), None) {
        Ok(ret) => return r.put(ret),
        Err(retry) => retry,
    };
    r.put(retry.await.0);
}

/// A fragment's task, started by @start: queued if it's a @reconstruct - the
/// retry makes a transaction of its own, and this thread is in one - else
/// inline, @start(true), till it waits on its IO.
fn start_task(t: &TransAttempt<'_, '_>, reconstruct: bool, start: impl FnOnce(bool)) {
    if reconstruct {
        return start(false);
    }
    // submit_bio() can block for an unbounded time on a congested
    // device; flag it so the long srcu hold that results doesn't trip
    // the warning in bch2_trans_unlock_long() — it's legitimate IO,
    // not a stuck codepath.
    t.trans().set_srcu_io_submitted(true);
    start(true);
    // We just submitted IO which may block, we expect relock fail
    // events and shouldn't count them:
    t.trans().set_notrace_relock_fail();
}

/// An update's read of @k, its first fragment made into @f.
#[inline(never)]
fn update_fragment<'r>(rbio: &'r Rbio, k: &BkeyI, btree: Btree, buf: BioRange<'r>, flags: F, dev: c_uint,
                       f: &mut Option<Fragment<'r>>) -> Result<(), BchError>
{
    let e = Extent { read_pos: k.k.start_pos(), btree, k: BkeySC::from(k), offset: 0, sectors: buf.len() >> 9 };
    let trans = btree_trans!(rbio.fs());
    lockrestart_do(&trans, |t| {
        if let Some(p) = fragment_of(t, rbio, buf, e, None, flags, false, dev)? {
            p.make(rbio, f)?;
        }
        Ok(())
    })
}

/// The update's read once @f's done: done, or @f's retry, to await.
#[inline(never)]
fn update_end<'r>(rbio: &'r Rbio, k: &'r BkeyI, btree: Btree, buf: BioRange<'r>, f: &mut Option<Fragment<'r>>,
                  ret: Result<(), BchError>) -> Result<ReadDone, RetryBox<'r>>
{
    let f = f.take().expect("an update's fragment, ended once");
    let pick = f.plan.landed.then_some(f.plan.pick);
    f.end(rbio, ret, Locate::Key(k, btree), Some(buf)).map(|ret| ReadDone { ret, pick, ptrs_io_error: 0, poisoned: None })
}

#[args_stored_once]
pub async fn read_update(rbio: &mut Rbio, k: &BkeyI, btree: Btree, flags: F, dev: c_uint) -> ReadDone {
    let buf = rbio.bio().range();
    let read = 'read: {
        // the fragment's gone before the retry's await: a future keeps a
        // local's slot till its scope ends, moved out of or not
        let retry = {
            let mut f = None;
            if let Err(e) = update_fragment(rbio, k, btree, buf, flags, dev, &mut f) {
                break 'read ReadDone { ret: Err(e), pick: None, ptrs_io_error: 0, poisoned: None };
            }
            let Some(frag) = f.as_mut() else {
                break 'read ReadDone { ret: Ok(()), pick: None, ptrs_io_error: 0, poisoned: None };
            };
            let ret = if frag.plan.pick.do_ec_reconstruct {
                Ok(())
            } else {
                let target = resume_target(frag);
                fragment_io(frag, rbio, target, None).await
            };
            if ret.is_err() {
                rbio.saw_error.store(true, Relaxed);
            }
            match update_end(rbio, k, btree, buf, &mut f, ret) {
                Ok(done) => break 'read done,
                Err(retry) => retry,
            }
        };
        let (ret, failures) = retry.await;
        ReadDone { ret, pick: failures.read, ptrs_io_error: ptrs_io_error(rbio.fs(), BkeySC::from(k), &failures.io),
                   poisoned: failures.poisoned }
    };
    if read.ret.is_err() {
        rbio.set_ret(read.ret);
    }
    rbio.done();
    read
}

/// What a read of the page cache does besides; for any other read, nothing.
pub trait ReadHooks {
    /// Extents ending at or before this sector are read already, and skipped.
    fn read_from(&self) -> u64 {
        0
    }

    /// @k runs @sectors past where the walk is, beyond the read bio's end:
    /// what's added to the bio is read too.
    fn extend(&mut self, _t: &TransAttempt<'_, '_>, _read: &Rbio, _k: BkeySC<'_>, _sectors: u32)
        -> Result<(), BchError>
    {
        Ok(())
    }

    /// @range, of @read's bio, is about to be read, of @k.
    fn extent(&mut self, _read: &Rbio, _range: BioRange<'_>, _k: BkeySC<'_>) {}

    /// A transaction restart, @read's bi_iter where the walk stopped: true if
    /// the read ends with what it's issued.
    fn give_up(&mut self, _read: &Rbio) -> bool {
        false
    }
}

impl ReadHooks for () {}

#[args_stored_once]
pub async fn read<H: ReadHooks>(orig: &Rbio, inum: c::subvol_inum, flags: F, mut hooks: H) -> Result<(), BchError> {
    stack_pin_init!(let reads = Reads::new());
    let mut own = None;
    let walked = read_walk(orig, inum, flags, &mut hooks, &reads, &mut own);
    {
        // the read's own bio's task: the fragment in the task's future alone,
        // made where it lives, and the future gone before the wait below
        stack_pin_init!(let own = unsafe {
            ::pin_init::pin_init_from_closure(|slot: *mut Option<FragmentFut<'_>>| {
                slot.write(own.map(|f| fragment_task(reads.get(), f, orig)));
                Ok::<_, core::convert::Infallible>(())
            })
        });
        if let Some(own) = own.as_pin_mut() {
            own.await;
        }
    }
    let ret = reads.wait(Resume::SPAWNED).await;
    orig.done();
    walked.and(ret).and(orig.ret())
}

/// read()'s walk of the extents it reads: each fragment's task started as it's
/// made, but for the read's own bio's, made into @own.
#[inline(never)]
fn read_walk<'r, H: ReadHooks>(orig: &'r Rbio, inum: c::subvol_inum, flags: F, hooks: &mut H, reads: &Reads,
                               own: &mut Option<Fragment<'r>>) -> Result<(), BchError> {
    let fs = orig.fs();
    let trans = btree_trans!(fs);
    let mut walk = Walk { read: orig, pos: BioIter, flags, split: false };
    let mut iter = BtreeIter::new(&trans, Btree::extents, pos(inum.inum, walk.range().pos().bi_sector),
                                  BtreeIterFlags::SLOTS);
    let ret = for_each_in_subvolume_max(&trans, &mut iter, pos(inum.inum, u64::MAX), inum.subvol as u32,
                                        BtreeIterFlags::SLOTS, |t, iter, k| {
        let step = extent_of(t, iter.pos(), k).and_then(|e| {
            let end = e.read_pos.offset + u64::from(e.sectors);
            if end <= hooks.read_from() {
                return Ok(Step::More(((end - e.read_pos.offset) << 9) as u32));
            }
            if walk.range().len() >> 9 < e.sectors {
                hooks.extend(t, orig, e.k, e.sectors)?;
            }
            let (range, step) = walk.fragment(&e);
            hooks.extent(orig, range, e.k);
            // the read's own bio, the walk's last, into @own: read by read(),
            // once it's done
            let started = fragment_of(t, orig, range, e, None, walk.flags, walk.split, u32::MAX)
                .and_then(|p| p.map_or(Ok(()), |p| p.start(t, orig, reads, step != Step::Last, &mut *own)));
            match started {
                Ok(()) => {}
                // the extent's error is the read's, and the walk goes on
                Err(e) if !e.matches(err::BCH_ERR_transaction_restart) => orig.set_ret(Err(e)),
                Err(e) => return Err(e),
            }
            Ok(step)
        });
        match step {
            Err(e) if e.matches(err::BCH_ERR_transaction_restart) => {
                if !hooks.give_up(orig) {
                    return Err(e);
                }
                t.restarts_handled();
                Ok(LoopFlow::Break(()))
            }
            step => match walk.step(step) {
                ControlFlow::Break(ret) => ret.map(LoopFlow::Break),
                ControlFlow::Continue(()) => Ok(LoopFlow::ContinueFrom(pos(inum.inum, walk.range().pos().bi_sector))),
            },
        }
    });

    if let Err(e) = ret {
        if !e.matches(err::BCH_ERR_extent_poisoned) {
            let mut m = log_msg_ratelimited!(fs);
            read_err_msg(&trans, &mut m, orig, pos(inum.inum, walk.range().pos().bi_sector));
            write!(m, "data read error: {e}");
        }
    }
    ret
}
