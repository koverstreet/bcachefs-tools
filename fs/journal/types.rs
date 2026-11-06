// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::fs::{Dev, Fs};
use crate::cstructs::c;
use crate::data::ptrs::IoFailures;
use crate::types::{CacheAligned, DArray, Fifo, RustOnly, c_default, rust_only};
use crate::util::alloc::KVVec;
use crate::util::projections::Volatile;
use crate::util::vstructs::Head;
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::sync::atomic::{AtomicI64, AtomicPtr, AtomicU64};
use crate::journal::buckets::JournalBuckets;
use crate::data_structures::list::ListLinks;
use crate::locking::{Locked, LockedBy};
use core::cell::Cell;
use crate::util::late::Late;
use crate::{new_lock, published};
use pin_init::pin_data;
use crate::util::workqueue::{Workqueue, WqFlags};
use cstruct_macros::{bitfield, c_const, c_enum, c_typedef, c_xmacro, CStruct, Init, Late};
use nestify::nest;

c_const! {
    /* btree write buffer steals 8 bits for its own purposes: */
    pub const JOURNAL_SEQ_MAX: u64 = (1 << 56) - 1;

    #[c_int]
    pub const JOURNAL_STATE_BUF_BITS: u32 = 2;
    pub const JOURNAL_STATE_BUF_NR: u32 = 1 << c::JOURNAL_STATE_BUF_BITS;
    pub const JOURNAL_STATE_BUF_MASK: u32 = c::JOURNAL_STATE_BUF_NR - 1;
}

/*
 * One journal buffer: staging area for a journal entry. Dynamically
 * allocated per journal entry (one per seq), rides j->in_flight from
 * the moment the entry is opened until its write completes and
 * seq_ondisk advances past it.
 *
 * Reservation concurrency is bounded by JOURNAL_STATE_BUF_NR via the
 * reservation state encoded in j->reservations; in-flight depth is
 * bounded only by the in_flight FIFO capacity (grown on demand) and
 * natural memory/device backpressure.
 */
nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct journal_buf {
        pub io: c::closure,
        pub j: *mut c::journal, /* for container_of-equivalent recovery */
        pub data: *mut Head<c::jset>,

        pub key: c::bkey_i,
        pub key_pad: [u64; c::BCH_REPLICAS_MAX as usize],
        /*
         * @cas mirrors the dev ptrs in @key in append order: cas[i] is the
         * bch_dev * for which __journal_write_alloc holds an io_ref.  Stashed
         * so the alloc → submit (or alloc → no_io) gap doesn't have to
         * re-derive ca via c->devs[idx], which dev_remove may have cleared
         * while our io_ref still pins the dev object.
         */
        pub cas: [*mut Dev; c::BCH_REPLICAS_MAX as usize],
        pub devs_written: c::bch_devs_list,
        pub failed: c::bch_io_failures,
        /* preflushes go to every rw member with unflushed writes */
        pub flush_failed: IoFailures<{ c::BCH_SB_MEMBERS_MAX as usize }>,

        pub last_seq: u64, /* copy of data->last_seq */

        pub buf_size: c_uint, /* size in bytes of @data */
        pub sectors: c_uint, /* maximum size for current entry */
        pub disk_sectors: c_uint, /* maximum size entry could have been, if
                                                buf_size was bigger */
        pub u64s_reserved: c_uint,

        #[c_bitfield]
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
        pub wait: c::closure_waitlist,
    }
}
c_default!(journal_buf);

/*
 * Something that makes a journal entry dirty - i.e. a btree node that has to be
 * flushed:
 */
c_enum! {
    #[closed]
    pub enum journal_pin_type: u32 {
        JOURNAL_PIN_TYPE_btree3,
        JOURNAL_PIN_TYPE_btree2,
        JOURNAL_PIN_TYPE_btree1,
        JOURNAL_PIN_TYPE_btree0,
        JOURNAL_PIN_TYPE_key_cache,
        JOURNAL_PIN_TYPE_other,
        JOURNAL_PIN_TYPE_NR,
    }
}

c_typedef! {
    pub type journal_pin_flush_fn = Option<unsafe extern "C" fn(j: *mut c::journal, *mut c::journal_entry_pin,
                                                                u64) -> c_int>;
}

/* A pin's, its seq's pin list lock's: the list it's on, and what it's on it as */
pub struct PinLocked {
    pub list:     ListLinks,
    pub flush:    Cell<c::journal_pin_flush_fn>,
    pub pin_type: Cell<c::journal_pin_type>,
}

// Zeroed, it's a pin never set: on no list, no flush function, JOURNAL_PIN_TYPE_btree3
unsafe impl ::pin_init::Zeroable for PinLocked {}

pub type PinLockedBy = rust_only!(LockedBy<PinLocked>);

#[repr(C)]
#[derive(CStruct)]
pub struct journal_entry_pin {
    pub locked: PinLockedBy,
    /* the entry it holds, 0 for none */
    pub seq:    Volatile<u64>,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_res {
    pub ref_: bool,
    pub has_overwrites: bool,
    pub u64s: u16,
    pub offset: u32,
    pub seq: u64,
}

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]*
    pub union journal_res_state {
        #[c_anon]
        pub atomic: pub struct journal_res_state_atomic {
            pub counter: c::atomic64_t,
        },

        #[c_anon]
        pub word: pub struct journal_res_state_word {
            pub v: u64,
        },

        /*
         * The bit layout within the word is the same on both byte orders:
         * cur_entry_offset at bit 0, idx at 22, then the four counts at bit
         * 24 + idx*10. That invariant lets journal_state_count() / _inc() /
         * _buf_put() index a count with a single shift, not a switch.
         */
        #[c_anon]
        pub fields: pub struct journal_res_state_fields {
            #[c_bitfield]
            #>[derive(Clone, Copy, CStruct)]-
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

c_const! {
    #[c_int]
    pub const JOURNAL_STATE_BUF_COUNT_BITS: u32 = 10; /* matches bufN_count:10 above */
    pub const JOURNAL_STATE_BUF_COUNT_MAX: u32 = (1 << c::JOURNAL_STATE_BUF_COUNT_BITS) - 1;
    #[c_int]
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
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_space {
    /* Units of 512 bytes sectors: */
    pub next_entry: c_uint, /* How big the next journal entry can be */
    pub total: c_uint,
}

c_enum! {
    #[closed]
    pub enum journal_space_from: u32 {
        journal_space_discarded,
        journal_space_clean_ondisk,
        journal_space_clean,
        journal_space_total,
        journal_space_nr,
    }
}

c_xmacro! {
    JOURNAL_FLAGS(x) {
        (degraded),
        (replay_done),
        (running),
        (may_skip_flush),
        (need_flush_write),
        (med_on_space),
        (low_on_space),
        (low_on_pin),
        (low_on_wb),
        (stuck_reported),
    }
}

c_enum! {
    #[closed]
    pub enum journal_flags: u32 {
        JOURNAL_FLAGS(n) => concat(JOURNAL_, n),
    }
}

#[repr(C)]
#[derive(CStruct)]
pub struct journal_bio {
    pub ca: *mut Dev,
    pub buf: *mut c::journal_buf,
    pub submit_time: u64,

    pub bio: c::bio,
}
c_default!(journal_bio);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_rewind_range {
    pub from: u64,
    pub to: u64,
}

/* j->lock's: reached only with it held */
#[repr(C)]
#[derive(CStruct)]
pub struct JournalLocked {
    pub cur_entry_sectors: c_uint,
    /*
     * 0, or -ENOSPC if waiting on journal reclaim, or -EROFS if
     * insufficient devices:
     */
    pub cur_entry_error: c_int,
    pub cur_entry_offset_if_blocked: c_uint,
    /* if nonzero, we may not open a new journal entry: */
    pub blocked: c_uint,
    pub flushes_outstanding: c_uint,

    pub seq_write_started: u64,
    pub last_empty_seq: u64,

    /*
     * The journal_buf of each ring slot, indexed by seq &
     * JOURNAL_STATE_BUF_MASK. Updated in journal_entry_open() when a new
     * seq is assigned to the slot's state index. Stale slots are never
     * dereferenced: the one reader is journal_cur_buf(), of the current
     * entry's slot. All seq→buf lookups go through j->in_flight.
     */
    pub ring_buf: [*mut c::journal_buf; c::JOURNAL_STATE_BUF_NR as usize],

    pub free_buf: *mut c_void,
    pub free_buf_size: c_uint,

    pub dirty_entry_bytes: usize,
    pub space: [c::journal_space; c::journal_space_nr as usize],
    pub can_discard: bool,
    pub last_flush_write: c_ulong,
    pub nr_flush_writes: u64,
    pub nr_noflush_writes: u64,
}

published! {
    /// Everything a reservation reads, in one cacheline, beside the counter
    /// its cmpxchg takes the line exclusive for.
    #[derive(CStruct)]
    pub struct JournalHot {
        pub __align: CacheAligned,
        #[c("union journal_res_state reservations")]
        pub reservations: AtomicU64,
        /*
         * seq, as reservations read it: a copy, set with it, so seq's own
         * readers don't share this line with every reservation's cmpxchg
         */
        pub res_seq: AtomicI64,
        /* Max size of current journal entry */
        pub cur_entry_u64s: Volatile<c_uint>,
        #[c("enum bch_watermark watermark")]
        pub watermark: Volatile<u32>,
        /*
         * Each open reservation's ring slot, indexed by seq &
         * JOURNAL_STATE_BUF_MASK: its journal_buf's data, and whether the
         * entry has overwrites - copies, so a reservation doesn't touch the
         * journal_buf. Set with ring_buf, in journal_entry_open().
         */
        #[c("struct jset *ring_data[JOURNAL_STATE_BUF_NR]")]
        pub ring_data: [AtomicPtr<Head<c::jset>>; c::JOURNAL_STATE_BUF_NR as usize],
        pub ring_has_overwrites: [Volatile<bool>; c::JOURNAL_STATE_BUF_NR as usize],
    }
}

published! {
    /// j->lock's, written under it and read without it.
    #[derive(CStruct)]
    pub struct JournalPublished {
        #[c_anon]
        pub hot: JournalHot,

        /* Sequence number of most recent journal entry (last entry in @pin) */
        pub seq: AtomicI64,

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
    }
}

/* pin_resize_lock's: the journal pin fifo's entries, a power of two of them */
#[repr(C)]
#[derive(CStruct)]
pub struct PinBuf {
    pub entries: rust_only!(KVVec<crate::journal::pins::PinList>),
}

/*
 * The fifo's live entries, [pin_front, pin_back), and how many there may be:
 * j->lock's, and pin_resize_lock's writer's while it replaces the buffer
 */
#[repr(C)]
#[derive(CStruct)]
pub struct PinRange {
    pub pin_front: Volatile<u64>,
    pub pin_back:  Volatile<u64>,
    pub pin_size:  Volatile<u64>,
}

/* Embedded in struct bch_fs */
nest! {
    #[pin_data]
    #[derive(CStruct)]*
    #[derive(Init, Late)]
    #[repr(C)]*
    pub struct journal {
        pub __align: CacheAligned,

        #[pin]
        #[init(new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&j->lock"))]
        #[c_lock("spinlock_t lock")]
        pub lock: Locked<c::spinlock_t, JournalLocked, JournalPublished>,

        #[c("unsigned long flags")]
        pub flags: crate::data_structures::bitmap::AtomicBitflags<c::journal_flags>,
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        pub stop_thread: *mut c::task_struct,

        pub buf_size_want: c_uint,
        /*
         * We may queue up some things to be journalled (log messages) before
         * the journal has actually started - stash them here:
         */
        pub early_journal_entries: c::darray_u64,

        /*
         * Protects journal_buf->data, when accessing without a jorunal
         * reservation: for synchronization between the btree write buffer code
         * and the journal write path:
         */
        pub buf_lock: c::mutex_noio,

        /*
         * FIFO of in-flight journal bufs, one entry per seq in
         * (seq_ondisk, cur_seq]. fifo.front = seq_ondisk + 1, fifo.back =
         * cur_seq + 1, so seq-indexed lookup is O(1) via fifo_entry().
         * Bufs live inline in the FIFO's backing array: pushed (and zeroed)
         * in journal_entry_open(), freed (front-advanced) in
         * journal_write_done() as seq_ondisk advances.
         */
        #[c("FIFO_U64_IDX(struct journal_buf) in_flight")]
        pub in_flight: Fifo<c::journal_buf, u64>,

        /*
         * When we need a flush but no open journal entry was flushable, wait
         * here - transferred to journal_buf.wait on entry open
         */
        pub flush_wait: c::closure_waitlist,

        /* Used when waiting because the journal was full */
        pub async_wait: c::closure_waitlist,
        pub reclaim_flush_wait: c::closure_waitlist,

        pub write_work: c::delayed_work,
        #[late(Workqueue::new(c"bcachefs_journal",
                              WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::PERCPU, 512),
               ENOMEM_fs_other_alloc)]
        #[c("struct workqueue_struct *wq")]
        pub wq: Late<Workqueue>,
        #[late(Workqueue::new(c"bcachefs_journal_discard",
                              WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::UNBOUND | WqFlags::MEM_RECLAIM, 512),
               ENOMEM_fs_other_alloc)]
        #[c("struct workqueue_struct *discard_wq")]
        pub discard_wq: Late<Workqueue>,

        pub flushed_seq_ondisk: Volatile<u64>,
        pub flushing_seq: AtomicI64,
        pub oldest_seq_found_ondisk: Volatile<u64>,

        /* Oldest journal seq that is safe to rewind to — discards of buckets
         * freed at >= this seq have not yet been issued */
        pub rewind_seq: Volatile<u64>,
        pub rewind_seq_ondisk: Volatile<u64>,

        /*
         * Rewind ranges: keys from journal entries with seq
         * in (to, from] use overwrite entries instead of
         * btree_keys entries.
         */
        pub rewind_ranges: DArray<c::journal_rewind_range>,

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
        #[init(new_lock!(PinBuf { entries: RustOnly::from_value(KVVec::new()) }, ::pin_init::zeroed(),
                         "&j->pin_resize_lock"))]
        #[c_lock("struct percpu_rw_semaphore pin_resize_lock")]
        pub pin_resize_lock: Locked<c::percpu_rw_semaphore, PinBuf, PinRange>,
        pub pin_resize_work: c::work_struct,

        /* Flushes under-replicated pins when a device comes back: */
        pub flush_degraded_work: c::work_struct,

        pub replay_journal_seq: Volatile<u64>,

        /*
         * The round robin over devices journal writes take - protected by
         * journal writes being allocated in order:
         */
        pub stripe: c::dev_stripe_state,
        /*
         * Failure domain keys scratch for journal_write_alloc() - as stripe:
         */
        pub wp_domain_keys: [u64; c::BCH_SB_MEMBERS_MAX as usize],
        /*
         * The devices the last journal write went to, which the next one
         * prefers - same protection as wp_domain_keys:
         */
        pub wp_prev_devs: c::bch_devs_mask,
        pub err_lock: c::spinlock_t,

        pub reclaim_lock: c::mutex,
        /*
         * Used for waiting until journal reclaim has freed up space in the
         * journal:
         */
        pub reclaim_wait: c::wait_queue_head_t,
        pub reclaim_thread: *mut c::task_struct,
        pub reclaim_kicked: bool,
        pub next_reclaim: c_ulong,
        pub nr_direct_reclaim: u64,
        pub nr_background_reclaim: u64,

        pub last_flushed: c_ulong,
        #[c("struct journal_entry_pin *flush_in_progress")]
        pub flush_in_progress: AtomicPtr<c::journal_entry_pin>,
        pub flush_in_progress_dropped: Volatile<bool>,
        /* its flush function, for messages: the pin can be freed under them */
        #[c("void *flush_in_progress_fn")]
        pub flush_in_progress_fn: AtomicPtr<c_void>,
        pub pin_flush_wait: c::wait_queue_head_t,

        pub write_start_time: u64,

        pub entry_bytes_written: u64,

        pub flush_write_time: *mut c::bch2_time_stats,
        pub noflush_write_time: *mut c::bch2_time_stats,
        pub flush_seq_time: *mut c::bch2_time_stats,

        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        pub res_map: c::lockdep_map,
    }
}
const _: () = assert!(core::mem::size_of::<JournalHot>() == core::mem::align_of::<JournalHot>(),
                      "JournalHot is more than a cacheline");

/* Where each stage of a journal bucket's life starts: journal/buckets.rs */
c_enum! {
    #[closed]
    pub enum journal_bucket_stage: u32 {
        JOURNAL_BUCKET_discard,
        JOURNAL_BUCKET_dirty_ondisk,
        JOURNAL_BUCKET_dirty,
        JOURNAL_BUCKET_cur,
    }
}

impl journal {
    pub fn fs(&self) -> &Fs {
        unsafe { &*core::ptr::from_ref(self).byte_sub(core::mem::offset_of!(Fs, journal)).cast() }
    }
}

/* Embedded in struct bch_dev */
#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct journal_device {
    #[pin]
    #[init(RustOnly::new(crate::journal::buckets::new()))]
    pub buckets: rust_only!(JournalBuckets),

    pub sectors_free: c_uint,

    /*
     * Bioset for journal write bios. Journal writes allocate from this at
     * submit time and free on completion; the pool size bounds the
     * mempool reserve, not in-flight depth.
     */
    pub bio_set: c::bio_set,

    pub discard_lock: c::mutex,
    pub discard: c::work_struct,

    pub highest_seq_found: u64,
}

unsafe impl Send for journal_device {}

/*
 * journal_entry_res - reserve space in every journal entry:
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_entry_res {
    pub u64s: c_uint,
}

/*
 * Computed by bch2_journal_read(), consumed by bch2_fs_recovery() and
 * bch2_fs_journal_start():
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
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_start_info {
    pub last_seq: u64,
    pub replay_end: u64,
    pub cur_seq: u64,
    pub clean: bool,
}
