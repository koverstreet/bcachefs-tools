// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::btree::bkey::{BkeyI, BKEY_U64S};
use crate::format::BkeyType;
use crate::fs::{Dev, Fs};
use crate::init::recovery::NotRw;
use crate::cstructs::c;
use crate::data_structures::dst_room::DstRoom;
use crate::types::CacheAligned;
use crate::journal::write::JournalWrite;
use crate::data::dev_bio::DevBio;
use crate::errcode::BchError;
use crate::os::bio::{BioRange, MappedPages};
use crate::util::alloc::{init_with, AllocGfp, Box, Flags, KVVec, Kmalloc};
use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::Printbuf;
use crate::util::printbuf::HumanReadable;
use crate::prt;
use core::mem;
use core::ptr::NonNull;
use crate::data_structures::fifo::FifoRead;
use crate::util::place::{Contained, Placed};
use crate::util::projections::Volatile;
use crate::util::refcount::{AtomicRef, RefGuard};
use crate::util::vstructs::{cast_plain, cast_plain_mut, plain_bytes, plain_bytes_mut, Head, SelfSized, Unvalidated};
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::sync::atomic::{AtomicI64, AtomicPtr, AtomicU64};
use crate::journal::buckets::{drop_retired, JournalBuckets};
use crate::journal::pins::{flush_degraded, pin_resize};
use crate::journal::reclaim::discard_work;
use crate::data_structures::list::ListLinks;
use crate::locking::{ExclusiveGuard, Guard as _, Locked, LockedBy, Mutex, MutexNoio, Noio, PercpuRwSem, SpinLockIrq};
use core::cell::Cell;
use crate::os::bio::BioSet;
use crate::util::futex::EventCount;
use crate::os::kthread::Daemon;
use crate::util::late::Late;
use crate::{new_lock, published};
use pin_init::pin_data;
use crate::journal::journal::journal_write_work;
use crate::os::workqueue::{DelayedWork, OnQueue, ReffedWork, Workqueue, WqFlags};
use cstruct_macros::{bitfield, bch_enum, placed, MaybeZeroable, Init, Late};
use nestify::nest;

/* btree write buffer steals 8 bits for its own purposes: */
pub const JOURNAL_SEQ_MAX: u64 = (1 << 56) - 1;

pub const JOURNAL_STATE_BUF_BITS: u32 = 2;
pub const JOURNAL_STATE_BUF_NR: u32 = 1 << c::JOURNAL_STATE_BUF_BITS;
pub const JOURNAL_STATE_BUF_MASK: u32 = c::JOURNAL_STATE_BUF_NR - 1;

/* journal bufs: entries open or in flight, at most */
pub const JOURNAL_BUF_NR: u32 = 256;

/// A journal entry's buffer, freed with it. Its pages a bio's own, in high
/// order allocations, which the CPU sees as one buffer: up to 4M, written by
/// clones of the bio - a few bvecs each - and page aligned, O_DIRECT in
/// userspace. Made with the write that writes it, so a write allocates
/// nothing: the two go from j->free_buf to an open entry's buf, side by side;
/// the buffer into the write as it starts, then its task, and back as it's
/// done.
pub type JsetBuf = DstRoom<c::jset<Unvalidated>, Box<MappedPages, Kmalloc>>;

impl JsetBuf {
    pub fn new(size: c_uint, gfp: Flags) -> Result<Self, BchError> {
        let pages = MappedPages::new(size as usize, gfp)?;
        Ok(DstRoom::from_storage(Box::into_unflagged(Box::init_in(init_with(|| pages), Kmalloc.with_flags(gfp))?)))
    }

    /// The bio its first @bytes are written from - by clones, which borrow it.
    pub fn bio_range(&self, bytes: u32) -> BioRange<'_> {
        self.storage().bio().range().take(bytes)
    }

    pub fn size(&self) -> c_uint {
        self.capacity() as c_uint
    }

    pub fn as_ptr(&self) -> *mut Head<c::jset> {
        self.storage().as_non_null().as_ptr().cast()
    }

    pub fn head(&self) -> &Head<c::jset> {
        cast_plain(self.bytes()).expect("a journal buffer, a jset's header at its start")
    }

    pub fn head_mut(&mut self) -> &mut Head<c::jset> {
        cast_plain_mut(self.bytes_mut()).expect("a journal buffer, a jset's header at its start")
    }

    /// The header, to write, while the body may be another's.
    ///
    /// # Safety
    /// j->lock is held, and nothing else refers to the header.
    unsafe fn header_mut(&self) -> &mut Head<c::jset> {
        unsafe { &mut *self.as_ptr() }
    }

    /// Where the body's first @u64s are, the entries, without the header: for
    /// the write buffer's flush, which has them by buf_lock.
    pub(crate) fn entries_ptr(&self, u64s: usize) -> NonNull<[u8]> {
        let hdr = mem::size_of::<Head<c::jset>>();
        let d = self.storage().as_non_null();
        assert!(hdr + u64s * 8 <= d.len(), "journal entries of {u64s} u64s past a buffer of {} bytes", d.len());
        let entries = d.cast::<u8>().map_addr(|a| a.checked_add(hdr).expect("a journal buffer's entries, within it"));
        NonNull::slice_from_raw_parts(entries, u64s * 8)
    }

    pub fn jset(&self) -> &c::jset<Unvalidated> {
        self
    }

    pub fn jset_mut(&mut self) -> &mut c::jset<Unvalidated> {
        self
    }

    /// The whole buffer, its jset and the room past it.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        plain_bytes_mut(self.elems_mut())
    }

    pub fn bytes(&self) -> &[u8] {
        plain_bytes(self.elems())
    }
}

/// An entry's buffer of @size, and the write that writes it.
pub fn jset_buf_alloc(size: c_uint, gfp: Flags) -> Result<(JsetBuf, MaybeRcuAlloc<JournalWrite>), BchError> {
    let buf = JsetBuf::new(size, gfp)?;
    let write = MaybeRcuAlloc::new_gfp(JournalWrite::new(), gfp)?;
    Ok((buf, write))
}

/// A buf's JsetBuf, had only as what's held allows: its header with j->lock;
/// its body with buf_lock, once the entry's reservations are put - till then
/// each reservation has its own range, through ring_data
/// (res_append_entries()). The JsetBuf itself is only put in, as the entry's
/// opened, and taken out, into its write as it starts.
#[derive(pin_init::Zeroable)]
pub struct BufJset(BufLocked<Option<JsetBuf>>);

impl BufJset {
    /// An entry's buffer, in, as it's opened.
    pub fn put(&self, g: &mut c::JournalGuard<'_>, data: JsetBuf) {
        let old = g.get_mut_of(&self.0).replace(data);
        assert!(old.is_none(), "a journal buf given an entry's buffer, holding another's");
    }

    /// Out, into its write as it starts.
    pub fn take(&self, g: &mut c::JournalGuard<'_>) -> JsetBuf {
        g.get_mut_of(&self.0).take().expect("a journal write's buffer, in its buf")
    }

    pub fn header<'g>(&'g self, g: &'g mut c::JournalGuard<'_>) -> &'g mut Head<c::jset> {
        let data = (*g).get_of(&self.0).as_ref().expect("an open journal entry's buffer");
        unsafe { data.header_mut() }
    }

    pub fn size(&self, g: &c::JournalGuard<'_>) -> c_uint {
        g.get_of(&self.0).as_ref().map_or(0, JsetBuf::size)
    }

    pub fn to_text(&self, g: &c::JournalGuard<'_>, out: &mut Printbuf) {
        if let Some(data) = g.get_of(&self.0) {
            prt!(out, "size:\t{}\n", HumanReadable(data.jset().bytes() as u64));
        }
    }

    /// Where its entries are, as many as its header says.
    pub(crate) fn entries_ptr(&self, g: &mut c::JournalGuard<'_>) -> NonNull<[u8]> {
        let u64s = self.header(g).u64s();
        g.get_of(&self.0).as_ref().expect("an open journal entry's buffer").entries_ptr(u64s)
    }
}

#[bitfield(u16)]
pub struct journal_buf_bits {
    separate_flush:             bool,
    need_flush_to_write_buffer: bool,
    write_started:              bool,
    write_allocated:            bool,
    write_done:                 bool,
    empty:                      bool,
    has_overwrites:             bool,
    __pad:                      u9,
}

/* A journal buf's, j->lock's */
pub type BufLocked<T> = LockedBy<T, (), JournalLockAt>;

pub type BufLockGuard<'a> = ExclusiveGuard<'a, Noio<Mutex>>;

/* A journal entry's extent key: a pointer for each device it's written to */
pub type JournalKey = DstRoom<BkeyI, [u64; BKEY_U64S + c::BCH_REPLICAS_MAX as usize]>;

impl JournalKey {
    pub fn empty() -> Self {
        let mut key = Self::default();
        key.k = c::bkey::new();
        key.k.set_key_type(BkeyType::extent);
        key
    }
}

/*
 * One journal buffer: staging area for a journal entry. A slot of
 * j->in_flight, made with the journal: an entry from the moment it's
 * opened until its write completes and seq_ondisk advances past it.
 *
 * Reservation concurrency is bounded by JOURNAL_STATE_BUF_NR via the
 * reservation state encoded in j->reservations; in-flight depth is
 * bounded by JOURNAL_BUF_NR, and natural memory/device backpressure.
 */
#[placed(InFlightLocked.bufs[], index = idx: u8)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct journal_buf {
    /* the entry's jset, till its write starts */
    pub data: BufJset,
    /* and the write, made with it */
    #[init(LockedBy::new(None, ()))]
    pub write: BufLocked<Option<MaybeRcuAlloc<JournalWrite>>>,
    /* its write's ref on the journal's writes_in_flight */
    #[init(LockedBy::new(None, ()))]
    pub in_flight: BufLocked<Option<JournalWriteRef>>,

    /* its write's, published as it's allocated */
    #[init(LockedBy::new(Default::default(), ()))]
    pub key: BufLocked<JournalKey>,

    pub last_seq: BufLocked<u64>, /* copy of data->last_seq */

    pub sectors: BufLocked<c_uint>, /* maximum size for current entry */
    pub disk_sectors: BufLocked<c_uint>, /* maximum size entry could have been, if
                                            buf_size was bigger */
    pub u64s_reserved: BufLocked<c_uint>,

    pub bits: BufLocked<journal_buf_bits>,

    /* must not be memset, only manipulated by xchg/cmpxchg */
    #[init(AtomicPtr::new(crate::journal::flush_wait::JOURNAL_BUF_NOT_IN_FLIGHT))]
    pub wait: AtomicPtr<crate::journal::flush_wait::FlushEntry>,
}

// its wait word only ever touched atomically, the rest had only as its
// LockedBys and BufJset let their holders have it, on any thread
unsafe impl Sync for journal_buf {}

impl journal_buf {
    /// Its journal, as 'static: a buf is retired before its journal goes, and
    /// what it hands out of it goes no further than the buf's write.
    pub fn journal(&self) -> &'static c::journal {
        self.container().journal()
    }
}

/*
 * The bufs of the entries open or in flight, a fifo by seq - locked(), with
 * j->lock held - whose front and back are the journal's: (seq_ondisk, seq],
 * which j->lock moves both ends of - an entry's opened at seq's increment,
 * and leaves as seq_ondisk passes it.
 */
#[placed(journal.in_flight)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct InFlight {
    #[pin]
    locked: InFlightLocked,
}

const _: () = assert!(JOURNAL_BUF_NR.is_power_of_two(), "a seq's buf is its low bits");

impl InFlight {
    pub fn journal(&self) -> &'static c::journal {
        unsafe { &*core::ptr::from_ref(self.container()) }
    }

    /// The buf @seq's entry has, if it's open or in flight - or another seq's:
    /// for what reads its wait word without j->lock.
    pub fn buf(&self, seq: u64) -> &journal_buf {
        &self.locked.bufs[seq as usize & (JOURNAL_BUF_NR as usize - 1)]
    }

    /// Its fifo, under the j->lock that moves its ends. Of the journal, not
    /// the guard: the bufs it hands out have fields the guard then writes.
    pub fn locked(&self, _: &c::JournalGuard<'_>) -> &InFlightLocked {
        &self.locked
    }
}

/// InFlight's bufs, as the fifo they are with j->lock held.
#[placed(InFlight.locked)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct InFlightLocked {
    #[pin]
    bufs: [journal_buf; JOURNAL_BUF_NR as usize],
}

impl InFlightLocked {
    fn journal(&self) -> &'static c::journal {
        self.container().journal()
    }
}

// every slot a journal_buf, made with the journal
unsafe impl FifoRead for InFlightLocked {
    type Elem = journal_buf;
    type Idx = u64;

    fn front_idx(&self) -> u64 {
        self.journal().lock.seq_ondisk.get() + 1
    }

    fn back_idx(&self) -> u64 {
        self.journal().fs().journal_cur_seq() + 1
    }

    fn capacity(&self) -> usize {
        JOURNAL_BUF_NR as usize
    }

    fn slot(&self, seq: u64) -> *const journal_buf {
        &self.bufs[seq as usize & (JOURNAL_BUF_NR as usize - 1)]
    }
}

/*
 * Something that makes a journal entry dirty - i.e. a btree node that has to be
 * flushed:
 */
#[bch_enum(closed)]
#[repr(u32)]
pub enum JournalPinType {
    JOURNAL_PIN_TYPE_btree3,
    JOURNAL_PIN_TYPE_btree2,
    JOURNAL_PIN_TYPE_btree1,
    JOURNAL_PIN_TYPE_btree0,
    JOURNAL_PIN_TYPE_key_cache,
    JOURNAL_PIN_TYPE_other,
    JOURNAL_PIN_TYPE_NR,
}

pub type PinFlushFn = fn(&Fs, &c::journal_entry_pin, u64) -> Result<(), crate::errcode::BchError>;

/* A pin's, its seq's pin list lock's: the list it's on, and what it's on it as */
pub struct PinLocked {
    pub list:     ListLinks,
    pub pin_type: Cell<c::JournalPinType>,
}

// Zeroed, it's a pin never set: on no list, JOURNAL_PIN_TYPE_btree3
unsafe impl ::pin_init::Zeroable for PinLocked {}

pub type PinLockedBy = LockedBy<PinLocked>;

#[derive(::pin_init::Zeroable)]
pub struct WriteAlloc {
    /* The round robin over devices journal writes take: */
    pub stripe: c::dev_stripe_state,
    /* Failure domain keys scratch for journal_write_alloc(): */
    pub wp_domain_keys: [u64; c::BCH_SB_MEMBERS_MAX as usize],
    /* The devices the last journal write went to, which the next one prefers: */
    pub wp_prev_devs: c::bch_devs_mask,
}

pub struct journal_entry_pin {
    pub locked: PinLockedBy,
    /* the entry it holds, 0 for none */
    pub seq:    Volatile<u64>,
    /* its holder's, set when it's made: it names the pin, in messages */
    pub(crate) flush: PinFlushFn,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct journal_res {
    pub ref_: bool,
    pub has_overwrites: bool,
    pub u64s: u16,
    pub offset: u32,
    pub seq: u64,
}

/// The journal's reservation state, one word: its fields are
/// journal_res_state_bits.
#[repr(transparent)]
#[derive(Clone, Copy, MaybeZeroable)]
pub struct journal_res_state {
    pub v: u64,
}

/*
 * The bit layout within the word is the same on both byte orders:
 * cur_entry_offset at bit 0, idx at 22, then the four counts at bit
 * 24 + idx*10. That invariant lets count() / inc() / state_buf_put()
 * index a count with a single shift, not a switch.
 */
#[bitfield(u64)]
pub struct journal_res_state_bits {
    cur_entry_offset: u22,
    idx:              u2,
    buf0_count:       u10,
    buf1_count:       u10,
    buf2_count:       u10,
    buf3_count:       u10,
}

pub const JOURNAL_STATE_BUF_COUNT_BITS: u32 = 10; /* matches bufN_count:10 above */
pub const JOURNAL_STATE_BUF_COUNT_MAX: u32 = (1 << c::JOURNAL_STATE_BUF_COUNT_BITS) - 1;
pub const JOURNAL_STATE_BUF0_SHIFT: u32 = 24; /* cur_entry_offset:22 + idx:2 */

/* bytes: */
pub const JOURNAL_ENTRY_SIZE_MIN: u32 = 64 << 10; /* 64k */

/*
 * The block layer is fragile with large bios - it should be able to process any
 * IO incrementally, but...
 *
 * 4MB corresponds to bio_kmalloc() -> UIO_MAXIOV
 */
pub const JOURNAL_ENTRY_SIZE_MAX: u32 = 4 << 20; /* 4M */

/*
 * We stash some journal state as sentinal values in cur_entry_offset:
 * note - cur_entry_offset is in units of u64s
 */
pub const JOURNAL_ENTRY_OFFSET_MAX: u32 = (1 << 22) - 1;

pub const JOURNAL_ENTRY_BLOCKED_VAL: u32 = c::JOURNAL_ENTRY_OFFSET_MAX - 2;
pub const JOURNAL_ENTRY_CLOSED_VAL: u32 = c::JOURNAL_ENTRY_OFFSET_MAX - 1;
pub const JOURNAL_ENTRY_ERROR_VAL: u32 = c::JOURNAL_ENTRY_OFFSET_MAX;

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct journal_space {
    /* Units of 512 bytes sectors: */
    pub next_entry: c_uint, /* How big the next journal entry can be */
    pub total: c_uint,
}

#[bch_enum(closed)]
#[repr(u32)]
pub enum JournalSpaceFrom {
    journal_space_discarded,
    journal_space_clean_ondisk,
    journal_space_clean,
    journal_space_total,
    journal_space_nr,
}

#[bch_enum(closed)]
#[repr(u32)]
pub enum JournalFlag {
    degraded,
    replay_done,
    running,
    may_skip_flush,
    need_flush_write,
    med_on_space,
    low_on_space,
    low_on_pin,
    low_on_wb,
    stuck_reported,
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct journal_rewind_range {
    pub from: u64,
    pub to: u64,
}

/* j->lock's: reached only with it held */
#[placed(journal.lock.*)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct JournalLocked {
    pub cur_entry_sectors: c_uint,
    pub cur_entry_offset_if_blocked: c_uint,
    pub flushes_outstanding: c_uint,

    pub seq_write_started: u64,
    pub last_empty_seq: u64,

    /* the next entry's buffer and write: both, or neither */
    pub free_buf:   Option<JsetBuf>,
    pub free_write: Option<MaybeRcuAlloc<JournalWrite>>,

    pub dirty_entry_bytes: usize,
    pub space: [c::journal_space; c::JournalSpaceFrom::journal_space_nr as usize],
    pub last_flush_write: c_ulong,
    pub nr_flush_writes: u64,
    pub nr_noflush_writes: u64,
    pub entry_bytes_written: u64,

    /*
     * We may queue up some things to be journalled (log messages) before
     * the journal has actually started - stash them here:
     */
    pub early_journal_entries: KVVec<u64>,
}

published! {
    /// Everything a reservation reads, in one cacheline, beside the counter
    /// its cmpxchg takes the line exclusive for.
    #[derive(MaybeZeroable)]
    pub struct JournalHot {
        pub __align: CacheAligned,
        pub reservations: AtomicU64,
        /*
         * seq, as reservations read it: a copy, set with it, so seq's own
         * readers don't share this line with every reservation's cmpxchg
         */
        pub res_seq: AtomicI64,
        /* Max size of current journal entry */
        pub cur_entry_u64s: Volatile<c_uint>,
        pub watermark: Volatile<u32>,
        /*
         * Each open reservation's ring slot, indexed by seq &
         * JOURNAL_STATE_BUF_MASK: its journal_buf's data, and whether the
         * entry has overwrites - copies, so a reservation doesn't touch the
         * journal_buf. Set in journal_entry_open().
         */
        pub ring_data: [AtomicPtr<Head<c::jset>>; c::JOURNAL_STATE_BUF_NR as usize],
        pub ring_has_overwrites: [Volatile<bool>; c::JOURNAL_STATE_BUF_NR as usize],
    }
}

published! {
    /// j->lock's, written under it and read without it.
    #[derive(MaybeZeroable)]
    pub struct JournalPublished {
        pub hot: JournalHot,

        /* Sequence number of most recent journal entry (last entry in @pin) */
        pub seq: AtomicI64,

        /*
         * 0, or -ENOSPC if waiting on journal reclaim, or -EROFS if
         * insufficient devices:
         */
        pub cur_entry_error: Volatile<c_int>,
        /* if nonzero, we may not open a new journal entry: */
        pub blocked: Volatile<c_uint>,
        pub can_discard: Volatile<bool>,

        /*
         * We have to ratelimit at the journal for the allocator because the
         * btree write buffer allocates btree nodes after the journal -
         * otherwise we'd get HOL blocking and priority inversion:
         */
        pub watermark_open_buckets: Volatile<u8>,

        /* Reserved space in journal entry to be used just prior to write */
        pub entry_u64s_reserved: Volatile<c_uint>,

        /* seq, last_seq from the most recent journal entry successfully written */
        pub seq_ondisk: Volatile<u64>,
        pub last_seq_ondisk: Volatile<u64>,
        pub err_seq: Volatile<u64>,
        pub last_seq: Volatile<u64>,
        pub replay_journal_seq_end: Volatile<u64>,

        pub flushed_seq_ondisk: Volatile<u64>,
        /* Oldest journal seq that is safe to rewind to — discards of buckets
         * freed at >= this seq have not yet been issued */
        pub rewind_seq: Volatile<u64>,
        pub rewind_seq_ondisk: Volatile<u64>,
        pub buf_size_want: Volatile<c_uint>,
    }
}

published! {
    /// reclaim_lock's: written by reclaim, read by anyone.
    #[derive(MaybeZeroable)]
    pub struct ReclaimPublished {
        pub last_flushed: Volatile<c_ulong>,
        pub nr_direct_reclaim: Volatile<u64>,
        pub nr_background_reclaim: Volatile<u64>,
    }
}

impl JournalPublished {
    /* No entry open */
    fn closed() -> Self {
        let p: Self = ::pin_init::Zeroable::zeroed();
        p.hot.reservations.store(c::JOURNAL_ENTRY_CLOSED_VAL.into(), core::sync::atomic::Ordering::Relaxed);
        p
    }
}

pub type JournalGuard<'a> =
    ExclusiveGuard<'a, SpinLockIrq, JournalLocked, JournalPublished, crate::locking::Exclusive, JournalLockAt>;

/// The journal lock's place, for what it covers to name.
pub type JournalLockAt = crate::at!(journal.lock);

type WritesInFlightAt = crate::at!(journal.writes_in_flight);

/// A journal write's ref on the journal, held to the end of write_done().
pub type JournalWriteRef = RefGuard<Placed<AtomicRef<()>, WritesInFlightAt>>;

/* pin_resize_lock's: the journal pin fifo's entries, a power of two of them */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct PinBuf {
    pub entries: KVVec<crate::journal::pins::PinList>,
}

/*
 * The fifo's live entries, [pin_front, pin_back), and how many there may be:
 * j->lock's, and pin_resize_lock's writer's while it replaces the buffer
 */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct PinRange {
    pub pin_front: Volatile<u64>,
    pub pin_back:  Volatile<u64>,
    pub pin_size:  Volatile<u64>,
}

/* The filesystem's write refs, from the journal: nest!() takes no macro in a type */
type FsWrites = crate::field!(super.writes);
/* And its ro refs, for what doesn't need it rw */
type FsRoRef = crate::field!(super.ro_ref);
/* And its own queue */
type JournalWq = OnQueue<crate::field!(wq)>;
pub type ReclaimThread = Daemon;
pub type RewindRanges = LockedBy<KVVec<c::journal_rewind_range>, (), NotRw<'static>>;

/* Embedded in struct bch_fs */
nest! {
    #[placed(Fs.journal)]
    #[pin_data]
    #[derive(MaybeZeroable)]*
    #[derive(Init, Late)]
    #[repr(C)]*
    pub struct journal {
        pub __align: CacheAligned,

        #[pin]
        #[init_locked(JournalPublished::closed(), "&j->lock")]
        pub lock: Placed<Locked<SpinLockIrq, JournalLocked, JournalPublished>, JournalLockAt>,

        #[pin]
        pub in_flight: InFlight,

        pub flags: crate::data_structures::bitmap::AtomicBitflags<c::JournalFlag>,
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        #[init(AtomicPtr::new((-(c::EROFS as isize)) as *mut c::task_struct))]
        pub stop_thread: AtomicPtr<c::task_struct>,

        /*
         * Protects journal_buf->data, when accessing without a jorunal
         * reservation: for synchronization between the btree write buffer code
         * and the journal write path:
         */
        #[pin]
        #[init(new_lock!((), "&j->buf_lock"))]
        pub buf_lock: MutexNoio,

        /*
         * When we need a flush but no open journal entry was flushable, wait
         * here - transferred to journal_buf.wait on entry open
         */
        pub flush_wait: AtomicPtr<crate::journal::flush_wait::FlushEntry>,

        /* Used when waiting because the journal was full */
        pub async_wait: EventCount,
        pub reclaim_flush_wait: EventCount,

        #[pin]
        pub(crate) write_work: DelayedWork<journal_write_work, JournalWq>,
        #[late(Workqueue::new(c"bcachefs_journal",
                              WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::PERCPU, 512),
               ENOMEM_fs_other_alloc)]
        pub wq: Late<Workqueue>,
        #[late(Workqueue::new(c"bcachefs_journal_discard",
                              WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::UNBOUND | WqFlags::MEM_RECLAIM, 512),
               ENOMEM_fs_other_alloc)]
        pub discard_wq: Late<Workqueue>,

        pub flushing_seq: AtomicI64,
        /* each write's ref, held to the end of write_done() - past waking its
         * flushers: the filesystem's exit waits them out */
        #[pin]
        #[init(AtomicRef::new(()))]
        pub writes_in_flight: Placed<AtomicRef<()>, WritesInFlightAt>,
        pub oldest_seq_found_ondisk: Volatile<u64>,

        /*
         * Rewind ranges: keys from journal entries with seq
         * in (to, from] use overwrite entries instead of
         * btree_keys entries.
         */
        pub rewind_ranges: RewindRanges,

        /*
         * FIFO of journal entries whose btree updates have not yet been
         * written out.
         *
         * Each entry is a reference count. The position in the FIFO is the
         * entry's sequence number relative to @seq.
         *
         * The journal entry itself holds a reference count, put when the
         * journal entry is written out. Each btree node modified by the journal
         * entry also holds a reference count, put when the btree node is
         * written.
         *
         * When a reference count reaches zero, the journal entry is no longer
         * needed. When all journal entries in the oldest journal bucket are no
         * longer needed, the bucket can be discarded and reused.
         */
        #[pin]
        #[init(new_lock!(PinBuf { entries: KVVec::new() }, ::pin_init::zeroed(),
                         "&j->pin_resize_lock"))]
        pub pin_resize_lock: Locked<PercpuRwSem, PinBuf, PinRange>,
        #[pin]
        pub(crate) pin_resize_work: ReffedWork<pin_resize, FsRoRef>,

        /* Flushes under-replicated pins when a device comes back: */
        #[pin]
        pub(crate) flush_degraded_work: ReffedWork<flush_degraded, FsRoRef>,

        /* Drops retired journal buckets reclaim is done with: buckets.rs */
        #[pin]
        pub(crate) drop_retired_work: ReffedWork<drop_retired, FsWrites>,

        pub replay_journal_seq: Volatile<u64>,

        /* the allocating write's: journal writes allocate one at a time, in order */
        #[pin]
        #[init(new_lock!(<- ::pin_init::init_zeroed(), (), "&j->write_alloc"))]
        pub write_alloc: Locked<Mutex, WriteAlloc>,

        #[pin]
        #[init(new_lock!((), ::pin_init::zeroed(), "&j->reclaim_lock"))]
        pub reclaim_lock: Locked<Mutex, (), ReclaimPublished>,
        /*
         * Used for waiting until journal reclaim has freed up space in the
         * journal:
         */
        pub reclaim_wait: EventCount,
        /* from reclaim_start() to reclaim_stop(): journal/reclaim.rs */
        #[pin]
        #[init(Daemon::new())]
        pub reclaim_thread: ReclaimThread,
        pub next_reclaim: Volatile<c_ulong>,

        pub flush_in_progress: AtomicPtr<c::journal_entry_pin>,
        pub flush_in_progress_dropped: Volatile<bool>,
        /* its flush function, for messages: the pin can be freed under them */
        pub flush_in_progress_fn: AtomicPtr<c_void>,
        /* Woken as a pin's flush ends: journal/reclaim.rs */
        pub pin_flush_wait: EventCount,

        pub write_start_time: Volatile<u64>,

        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        #[pin]
        #[init(crate::new_lockdep_map!("journal res"))]
        pub res_map: crate::locking::LockdepMap,
    }
}
const _: () = assert!(core::mem::size_of::<JournalHot>() == core::mem::align_of::<JournalHot>(),
                      "JournalHot is more than a cacheline");

/* Where each stage of a journal bucket's life starts: journal/buckets.rs */
#[bch_enum(closed)]
#[repr(u32)]
pub enum JournalBucketStage {
    JOURNAL_BUCKET_discard,
    JOURNAL_BUCKET_dirty_ondisk,
    JOURNAL_BUCKET_dirty,
    JOURNAL_BUCKET_cur,
}

impl journal {
    pub fn fs(&self) -> &Fs {
        self.container()
    }
}

/* Embedded in struct bch_dev */
#[placed(Dev.journal)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct journal_device {
    #[pin]
    #[init(crate::journal::buckets::new())]
    pub buckets: JournalBuckets,

    pub sectors_free: Volatile<c_uint>,

    /*
     * Bioset for journal write bios. Journal writes allocate from this at
     * submit time and free on completion; the pool size bounds the
     * mempool reserve, not in-flight depth.
     */
    pub bio_set: Late<BioSet<DevBio>>,

    #[pin]
    #[init(new_lock!((), "&ca->journal.discard_lock"))]
    pub discard_lock: Locked<Mutex>,
    #[pin]
    pub(crate) discard: ReffedWork<discard_work, crate::field!(super.io_ref_write)>,
}

/*
 * journal_entry_res - reserve space in every journal entry:
 */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct journal_entry_res {
    /* written by entry_res_resize(), under j->lock */
    pub u64s: Volatile<c_uint>,
}

/*
 * Computed by bch2_journal_read(), consumed by bch2_fs_recovery() and
 * journal.start():
 *
 * After journal read we have three sequence number zones:
 *
 *   [last_seq ... replay_end]  [replay_end+1 ... cur_seq-1]  [cur_seq ...]
 *         replay these              blacklist these            new writes
 *
 * @last_seq:	Start of replay window — from last flush entry's last_seq.
 *		All entries >= last_seq are needed for recovery.
 *
 * @replay_end:	End of replay window — last flush entry's seq.
 *		Entries past this may exist on disk (noflush/torn writes)
 *		but are unreliable.
 *
 * @cur_seq:	First sequence number available for new journal writes.
 *		Initialized to highest on-disk entry + 1, then bumped
 *		further by recovery (+64 for unclean
 *		shutdown) and max'd with last blacklisted seq.
 *		Must be strictly greater than every entry found on disk,
 *		including noflush/blacklisted entries — we must never
 *		reuse a sequence number that was already written.
 *
 * @clean:	Last flush entry was empty (filesystem was clean).
 */
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct journal_start_info {
    pub last_seq: u64,
    pub replay_end: u64,
    pub cur_seq: u64,
    pub clean: bool,
}
