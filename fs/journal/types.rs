// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::btree::bkey::BKEY_U64S;
use crate::fs::{Dev, Fs};
use crate::init::recovery::NotRw;
use crate::cstructs::c;
use crate::data::keylist::PaddedKey;
use crate::data::ptrs::IoFailures;
use crate::data::write::DevWriteRef;
use crate::types::{CacheAligned, c_default};
use crate::util::alloc::{kvfree_sized, kvzalloc_gfp, AllocError, KVVec};
use crate::data_structures::fixed_fifo::FixedFifo;
use crate::util::place::{Contained, Placed};
use crate::util::projections::Volatile;
use crate::util::vstructs::{from_head_ptr, from_head_ptr_mut, Head, Unvalidated};
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::sync::atomic::{AtomicI64, AtomicPtr, AtomicU64};
use crate::journal::buckets::{drop_retired, JournalBuckets};
use crate::journal::pins::{flush_degraded, pin_resize};
use crate::journal::reclaim::discard_work;
use crate::os::kernel::WRITE;
use crate::data_structures::list::ListLinks;
use crate::locking::{ExclusiveGuard, Locked, LockedBy};
use core::cell::Cell;
use crate::util::bio::BioSet;
use crate::util::futex::EventCount;
use crate::os::kthread::Daemon;
use crate::util::late::Late;
use crate::{new_lock, published};
use pin_init::pin_data;
use crate::os::workqueue::{DelayedWork, ReffedWork, Workqueue, WqFlags};
use cstruct_macros::{bitfield, bch_enum, placed, MaybeZeroable, Init, Late, BitfieldMembers};
use nestify::nest;

/* btree write buffer steals 8 bits for its own purposes: */
pub const JOURNAL_SEQ_MAX: u64 = (1 << 56) - 1;

pub const JOURNAL_STATE_BUF_BITS: u32 = 2;
pub const JOURNAL_STATE_BUF_NR: u32 = 1 << c::JOURNAL_STATE_BUF_BITS;
pub const JOURNAL_STATE_BUF_MASK: u32 = c::JOURNAL_STATE_BUF_NR - 1;

/* journal bufs: entries open or in flight, at most */
pub const JOURNAL_BUF_NR: u32 = 256;

/// A journal entry's buffer, freed with it. C moves it whole, data and size
/// together: between j->free_buf and the bufs in flight.
#[repr(C)]
pub struct JsetBuf {
    data: *mut Head<c::jset>,
    size: c_uint, /* size in bytes of @data */
}

unsafe impl ::pin_init::Zeroable for JsetBuf {}

impl JsetBuf {
    pub fn new(size: c_uint, gfp: c::gfp_t) -> Result<Self, AllocError> {
        Ok(JsetBuf { data: kvzalloc_gfp(size as usize, gfp)?.as_ptr().cast(), size })
    }

    pub fn size(&self) -> c_uint {
        self.size
    }

    pub fn as_ptr(&self) -> *mut Head<c::jset> {
        self.data
    }

    pub fn head(&self) -> &Head<c::jset> {
        unsafe { &*self.data }
    }

    pub fn head_mut(&mut self) -> &mut Head<c::jset> {
        unsafe { &mut *self.data }
    }

    pub fn jset(&self) -> &c::jset<Unvalidated> {
        unsafe { from_head_ptr(self.data) }
    }

    pub fn jset_mut(&mut self) -> &mut c::jset<Unvalidated> {
        unsafe { from_head_ptr_mut(self.data) }
    }

    /// The whole buffer, its jset and the room past it.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.data.cast(), self.size as usize) }
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.data.cast(), self.size as usize) }
    }
}

impl Default for JsetBuf {
    fn default() -> Self {
        JsetBuf { data: core::ptr::null_mut(), size: 0 }
    }
}

impl Drop for JsetBuf {
    fn drop(&mut self) {
        if let Some(data) = core::ptr::NonNull::new(self.data) {
            unsafe { kvfree_sized(data.cast(), self.size as usize) };
        }
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
nest! {
    #[placed(JournalLocked.in_flight[], index = idx: u8)]
    #[pin_data]
    #[repr(C)]
    #[derive(MaybeZeroable, BitfieldMembers, Init)]
    pub struct journal_buf {
        pub io: c::closure,
        pub data: JsetBuf,

        pub key: PaddedKey<{ BKEY_U64S + c::BCH_REPLICAS_MAX as usize }>,
        /*
         * @cas mirrors the dev ptrs in @key in append order: cas[i] is the
         * io_ref __journal_write_alloc took on that dev.  Stashed so the
         * alloc → submit (or alloc → no_io) gap doesn't have to re-derive ca
         * via c->devs[idx], which dev_remove may have cleared while our
         * io_ref still pins the dev object.
         */
        #[init(<[Option<DevWriteRef>; c::BCH_REPLICAS_MAX as usize]>::default())]
        pub cas: [Option<DevWriteRef>; c::BCH_REPLICAS_MAX as usize],
        pub devs_written: c::bch_devs_list,
        #[init(c::bch_io_failures::default())]
        pub failed: c::bch_io_failures,
        /* preflushes go to every rw member with unflushed writes */
        #[init(IoFailures::default())]
        pub flush_failed: IoFailures<{ c::BCH_SB_MEMBERS_MAX as usize }>,

        pub last_seq: u64, /* copy of data->last_seq */

        pub sectors: c_uint, /* maximum size for current entry */
        pub disk_sectors: c_uint, /* maximum size entry could have been, if
                                                buf_size was bigger */
        pub u64s_reserved: c_uint,

        #[members]
        #>[bitfield(u16)]
        pub bits: pub struct journal_buf_bits {
            separate_flush:             bool,
            need_flush_to_write_buffer: bool,
            write_started:              bool,
            write_allocated:            bool,
            write_done:                 bool,
            empty:                      bool,
            has_overwrites:             bool,
            __pad:                      u9,
        },

        /* must not be memset, only manipulated by xchg/cmpxchg */
        #[init(c::closure_waitlist { list: c::llist_head { first: c::JOURNAL_BUF_NOT_IN_FLIGHT } })]
        pub wait: c::closure_waitlist,
    }
}

impl journal_buf {
    /// Its journal, as 'static: a buf is retired before its journal goes, and
    /// what it hands out of it goes no further than the buf's write.
    pub fn journal(&self) -> &'static c::journal {
        unsafe { &*core::ptr::from_ref(self.container()) }
    }

    /// What it holds dropped, its write done: a free slot holds nothing, till
    /// an entry's opened on it.
    pub fn retire(&mut self) {
        self.data = Default::default();
        self.cas = Default::default();
        self.flush_failed = Default::default();
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

#[allow(non_camel_case_types)]
pub type journal_pin_flush_fn = Option<unsafe extern "C" fn(j: *mut c::journal, *mut c::journal_entry_pin,
                                                            u64) -> c_int>;

/* A pin's, its seq's pin list lock's: the list it's on, and what it's on it as */
pub struct PinLocked {
    pub list:     ListLinks,
    pub flush:    Cell<c::journal_pin_flush_fn>,
    pub pin_type: Cell<c::JournalPinType>,
}

// Zeroed, it's a pin never set: on no list, no flush function, JOURNAL_PIN_TYPE_btree3
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

pub type WriteAllocLockedBy = LockedBy<WriteAlloc>;

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct journal_entry_pin {
    pub locked: PinLockedBy,
    /* the entry it holds, 0 for none */
    pub seq:    Volatile<u64>,
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

nest! {
    #[derive(Clone, Copy, MaybeZeroable)]*
    #[repr(C)]*
    pub union journal_res_state {
        pub atomic: pub struct journal_res_state_atomic {
            pub counter: c::atomic64_t,
        },

        pub word: pub struct journal_res_state_word {
            pub v: u64,
        },

        /*
         * The bit layout within the word is the same on both byte orders:
         * cur_entry_offset at bit 0, idx at 22, then the four counts at bit
         * 24 + idx*10. That invariant lets count() / inc() / state_buf_put()
         * index a count with a single shift, not a switch.
         */
        #>[derive(BitfieldMembers)]
        pub fields: pub struct journal_res_state_fields {
            #[members]
            #>[derive(Clone, Copy, MaybeZeroable)]-
            #>[repr(C)]-
            #>[bitfield(u64)]
            pub bits: pub struct journal_res_state_bits {
                cur_entry_offset: u22,
                idx:              u2,
                buf0_count:       u10,
                buf1_count:       u10,
                buf2_count:       u10,
                buf3_count:       u10,
            },
        },
    }
}
c_default!(journal_res_state);

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
#[derive(MaybeZeroable)]
pub struct journal_bio {
    pub ca: *mut Dev,
    pub buf: *mut c::journal_buf,
    pub submit_time: u64,

    pub bio: c::bio,
}
c_default!(journal_bio);
crate::has_bio!(journal_bio, bio);

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

    pub free_buf: JsetBuf,

    pub dirty_entry_bytes: usize,
    pub space: [c::journal_space; c::JournalSpaceFrom::journal_space_nr as usize],
    pub last_flush_write: c_ulong,
    pub nr_flush_writes: u64,
    pub nr_noflush_writes: u64,
    pub entry_bytes_written: u64,

    /*
     * FIFO of in-flight journal bufs, one entry per seq in
     * (seq_ondisk, cur_seq]. fifo.front = seq_ondisk + 1, fifo.back =
     * cur_seq + 1, so seq-indexed lookup is O(1).
     * Bufs are the FIFO's slots: pushed (and reset) in
     * journal_entry_open(), popped in journal_write_done() as seq_ondisk
     * advances.
     *
     * The fifo is j->lock's; its bufs are used outside it, by their refs.
     */
    #[pin]
    pub in_flight: FixedFifo<c::journal_buf, { c::JOURNAL_BUF_NR as usize }, u64>,

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
    ExclusiveGuard<'a, c::spinlock_t, JournalLocked, JournalPublished, crate::locking::Exclusive, JournalLockAt>;

/// The journal lock's place, for what it covers to name.
pub type JournalLockAt = crate::at!(journal.lock);

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
        pub lock: Placed<Locked<c::spinlock_t, JournalLocked, JournalPublished>, JournalLockAt>,

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
        pub buf_lock: Locked<c::mutex_noio>,

        /*
         * When we need a flush but no open journal entry was flushable, wait
         * here - transferred to journal_buf.wait on entry open
         */
        pub flush_wait: c::closure_waitlist,

        /* Used when waiting because the journal was full */
        pub async_wait: c::closure_waitlist,
        pub reclaim_flush_wait: c::closure_waitlist,

        #[pin]
        #[init(crate::journal::journal::WriteWork::init())]
        pub write_work: DelayedWork,
        #[late(Workqueue::new(c"bcachefs_journal",
                              WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::PERCPU, 512),
               ENOMEM_fs_other_alloc)]
        pub wq: Late<Workqueue>,
        #[late(Workqueue::new(c"bcachefs_journal_discard",
                              WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::UNBOUND | WqFlags::MEM_RECLAIM, 512),
               ENOMEM_fs_other_alloc)]
        pub discard_wq: Late<Workqueue>,

        pub flushing_seq: AtomicI64,
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
        pub pin_resize_lock: Locked<c::percpu_rw_semaphore, PinBuf, PinRange>,
        #[pin]
        pub(crate) pin_resize_work: ReffedWork<pin_resize, FsRoRef>,

        /* Flushes under-replicated pins when a device comes back: */
        #[pin]
        pub(crate) flush_degraded_work: ReffedWork<flush_degraded, FsRoRef>,

        /* Drops retired journal buckets reclaim is done with: buckets.rs */
        #[pin]
        pub(crate) drop_retired_work: ReffedWork<drop_retired, FsWrites>,

        pub replay_journal_seq: Volatile<u64>,

        /* protected by journal writes being allocated in order: */
        #[pin]
        #[init(::pin_init::zeroed::<LockedBy<WriteAlloc>>())]
        pub write_alloc: WriteAllocLockedBy,
        #[pin]
        #[init(new_lock!((), "&j->err_lock"))]
        pub err_lock: Locked<c::spinlock_t>,

        #[pin]
        #[init(new_lock!((), ::pin_init::zeroed(), "&j->reclaim_lock"))]
        pub reclaim_lock: Locked<c::mutex, (), ReclaimPublished>,
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
    pub bio_set: Late<BioSet<journal_bio>>,

    #[pin]
    #[init(new_lock!((), "&ca->journal.discard_lock"))]
    pub discard_lock: Locked<c::mutex>,
    #[pin]
    pub(crate) discard: ReffedWork<discard_work, crate::field!(super.io_ref[WRITE])>,
}

unsafe impl Send for journal_device {}

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
