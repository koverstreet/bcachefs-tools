// SPDX-License-Identifier: GPL-2.0

//! The data types of journal/types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, Fifo, c_default};
use cstruct_macros::{bitfield, c_const, c_enum, c_typedef, c_verbatim, c_xmacro, CStruct};
use nestify::nest;

c_const! {
    /* btree write buffer steals 8 bits for its own purposes: */
    pub const JOURNAL_SEQ_MAX: u64 = (1 << 56) - 1;
}

c_const! {
    #[c_int]
    pub const JOURNAL_STATE_BUF_BITS: u32 = 2;
}

c_const! {
    pub const JOURNAL_STATE_BUF_NR: u32 = 1 << c::JOURNAL_STATE_BUF_BITS;
}

c_const! {
    pub const JOURNAL_STATE_BUF_MASK: u32 = c::JOURNAL_STATE_BUF_NR - 1;
}

c_verbatim!(r#"
struct journal;
"#);

#[bitfield(u16, repr = zerocopy::byteorder::native_endian::U16, from = zerocopy::byteorder::native_endian::U16::new, into = zerocopy::byteorder::native_endian::U16::get)]
pub struct journal_buf_flush_picked_bits {
    /* write has already been or waiting to be kicked off */
    #[bits(1)]
    pub flush_picked: bool,
    #[bits(1)]
    pub flush: bool,

    #[bits(1)]
    pub separate_flush: bool,
    #[bits(1)]
    pub need_flush_to_write_buffer: bool,
    #[bits(1)]
    pub write_started: bool,
    #[bits(1)]
    pub write_allocated: bool,
    #[bits(1)]
    pub write_done: bool,
    #[bits(1)]
    pub empty: bool,
    #[bits(1)]
    pub has_overwrites: bool,
    #[bits(7)]
    pub __pad: u8,
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
#[repr(C)]
#[derive(CStruct)]
pub struct journal_buf {
    pub io: c::closure,
    pub j: *mut c::journal, /* for container_of-equivalent recovery */
    pub data: *mut c::jset,

    pub key: c::bkey_i,
    pub key_pad: [u64; c::BCH_REPLICAS_MAX as usize],
    /*
     * @cas mirrors the dev ptrs in @key in append order: cas[i] is the
     * bch_dev * for which __journal_write_alloc holds an io_ref.  Stashed
     * so the alloc → submit (or alloc → no_io) gap doesn't have to
     * re-derive ca via c->devs[idx], which dev_remove may have cleared
     * while our io_ref still pins the dev object.
     */
    pub cas: [*mut c::bch_dev; c::BCH_REPLICAS_MAX as usize],
    pub devs_written: c::bch_devs_list,
    pub failed: c::bch_io_failures,

    pub last_seq: u64, /* copy of data->last_seq */

    pub buf_size: core::ffi::c_uint, /* size in bytes of @data */
    pub sectors: core::ffi::c_uint, /* maximum size for current entry */
    pub disk_sectors: core::ffi::c_uint, /* maximum size entry could have been, if
                                            buf_size was bigger */
    pub u64s_reserved: core::ffi::c_uint,

    #[c_bitfield]
    pub flush_picked_bits: journal_buf_flush_picked_bits,

    /* must not be memset, only manipulated by xchg/cmpxchg */
    pub wait: c::closure_waitlist,
}
c_default!(journal_buf);
impl journal_buf {
    pub fn flush_picked(&self) -> bool { let b = self.flush_picked_bits; b.flush_picked() }
    pub fn set_flush_picked(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_flush_picked(v); self.flush_picked_bits = b; }
    pub fn flush(&self) -> bool { let b = self.flush_picked_bits; b.flush() }
    pub fn set_flush(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_flush(v); self.flush_picked_bits = b; }
    pub fn separate_flush(&self) -> bool { let b = self.flush_picked_bits; b.separate_flush() }
    pub fn set_separate_flush(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_separate_flush(v); self.flush_picked_bits = b; }
    pub fn need_flush_to_write_buffer(&self) -> bool { let b = self.flush_picked_bits; b.need_flush_to_write_buffer() }
    pub fn set_need_flush_to_write_buffer(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_need_flush_to_write_buffer(v); self.flush_picked_bits = b; }
    pub fn write_started(&self) -> bool { let b = self.flush_picked_bits; b.write_started() }
    pub fn set_write_started(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_write_started(v); self.flush_picked_bits = b; }
    pub fn write_allocated(&self) -> bool { let b = self.flush_picked_bits; b.write_allocated() }
    pub fn set_write_allocated(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_write_allocated(v); self.flush_picked_bits = b; }
    pub fn write_done(&self) -> bool { let b = self.flush_picked_bits; b.write_done() }
    pub fn set_write_done(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_write_done(v); self.flush_picked_bits = b; }
    pub fn empty(&self) -> bool { let b = self.flush_picked_bits; b.empty() }
    pub fn set_empty(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_empty(v); self.flush_picked_bits = b; }
    pub fn has_overwrites(&self) -> bool { let b = self.flush_picked_bits; b.has_overwrites() }
    pub fn set_has_overwrites(&mut self, v: bool) { let mut b = self.flush_picked_bits; b.set_has_overwrites(v); self.flush_picked_bits = b; }
}

/*
 * Ring slot for open reservations. Cache of the journal_buf pointer and
 * its data pointer, indexed by seq & JOURNAL_STATE_BUF_MASK. The
 * reservation fastpath reads .data directly to avoid the extra
 * indirection through .buf.
 *
 * A ring slot is overwritten in journal_entry_open() when a new seq is
 * assigned to that state index. Stale entries are never dereferenced
 * because the only readers are reservation holders (trusted: the
 * reservation pins the buf) or journal_res_entry() via those reservations.
 * All non-reservation seq→buf lookups go through j->in_flight.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_ringbuf {
    pub buf: *mut c::journal_buf,
    pub data: *mut c::jset,
}
c_default!(journal_ringbuf);

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

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]*
    pub struct journal_entry_pin_list {
        pub lock: c::spinlock_t,
        pub count: c::atomic_t,
        pub unflushed: [c::list_head; c::JOURNAL_PIN_TYPE_NR as usize],
        pub flushed: c::list_head,
        pub unreplayed: bool,
        #[c_inline]
        pub devs: pub struct journal_entry_pin_list_devs {
            pub nr: u8,
            pub data: [u8; c::BCH_REPLICAS_MAX as usize],
        },
        pub bytes: core::ffi::c_uint,
    }
}
c_default!(journal_entry_pin_list);

c_verbatim!(r#"
struct journal;
struct journal_entry_pin;
"#);

c_typedef! {
    pub type journal_pin_flush_fn = Option<unsafe extern "C" fn(j: *mut c::journal, *mut c::journal_entry_pin,
                                                                u64) -> core::ffi::c_int>;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_entry_pin {
    pub list: c::list_head,
    pub flush: c::journal_pin_flush_fn,
    pub seq: u64,
}
c_default!(journal_entry_pin);

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
         * Field order is reversed on big-endian so the on-word bit layout is
         * identical on both: cur_entry_offset at bit 0, idx at 22, then the four
         * counts at bit 24 + idx*10. That invariant lets journal_state_count() /
         * _inc() / _buf_put() index a count with a single shift, not a switch.
         */
        #[c_anon]
        pub fields: pub struct journal_res_state_fields {
            #[c_bitfield]
            #>[derive(Clone, Copy, CStruct)]-
            #>[repr(C)]-
            #>[bitfield(u64)]
            pub cur_entry_offset_bits: pub struct journal_res_state_bits {
                #[bits(22)]
                pub cur_entry_offset: u64,
                #[bits(2)]
                pub idx: u64,
                #[bits(10)]
                pub buf0_count: u64,
                #[bits(10)]
                pub buf1_count: u64,
                #[bits(10)]
                pub buf2_count: u64,
                #[bits(10)]
                pub buf3_count: u64,
            },
        },
    }
}
c_default!(journal_res_state);
impl journal_res_state_fields {
    pub fn cur_entry_offset(&self) -> u64 { let b = self.cur_entry_offset_bits; b.cur_entry_offset() }
    pub fn set_cur_entry_offset(&mut self, v: u64) { let mut b = self.cur_entry_offset_bits; b.set_cur_entry_offset(v); self.cur_entry_offset_bits = b; }
    pub fn idx(&self) -> u64 { let b = self.cur_entry_offset_bits; b.idx() }
    pub fn set_idx(&mut self, v: u64) { let mut b = self.cur_entry_offset_bits; b.set_idx(v); self.cur_entry_offset_bits = b; }
    pub fn buf0_count(&self) -> u64 { let b = self.cur_entry_offset_bits; b.buf0_count() }
    pub fn set_buf0_count(&mut self, v: u64) { let mut b = self.cur_entry_offset_bits; b.set_buf0_count(v); self.cur_entry_offset_bits = b; }
    pub fn buf1_count(&self) -> u64 { let b = self.cur_entry_offset_bits; b.buf1_count() }
    pub fn set_buf1_count(&mut self, v: u64) { let mut b = self.cur_entry_offset_bits; b.set_buf1_count(v); self.cur_entry_offset_bits = b; }
    pub fn buf2_count(&self) -> u64 { let b = self.cur_entry_offset_bits; b.buf2_count() }
    pub fn set_buf2_count(&mut self, v: u64) { let mut b = self.cur_entry_offset_bits; b.set_buf2_count(v); self.cur_entry_offset_bits = b; }
    pub fn buf3_count(&self) -> u64 { let b = self.cur_entry_offset_bits; b.buf3_count() }
    pub fn set_buf3_count(&mut self, v: u64) { let mut b = self.cur_entry_offset_bits; b.set_buf3_count(v); self.cur_entry_offset_bits = b; }
}

c_const! {
    #[c_int]
    pub const JOURNAL_STATE_BUF_COUNT_BITS: u32 = 10; /* matches bufN_count:10 above */
}

c_const! {
    pub const JOURNAL_STATE_BUF_COUNT_MAX: u32 = (1 << c::JOURNAL_STATE_BUF_COUNT_BITS) - 1;
}

c_const! {
    #[c_int]
    pub const JOURNAL_STATE_BUF0_SHIFT: u32 = 24; /* cur_entry_offset:22 + idx:2 */
}

c_const! {
    /* bytes: */
    pub const JOURNAL_ENTRY_SIZE_MIN: u32 = 64 << 10; /* 64k */
}

c_const! {
    /*
     * The block layer is fragile with large bios - it should be able to process any
     * IO incrementally, but...
     *
     * 4MB corresponds to bio_kmalloc() -> UIO_MAXIOV
     */
    pub const JOURNAL_ENTRY_SIZE_MAX: u32 = 4 << 20; /* 4M */
}

c_const! {
    /*
     * We stash some journal state as sentinal values in cur_entry_offset:
     * note - cur_entry_offset is in units of u64s
     */
    pub const JOURNAL_ENTRY_OFFSET_MAX: u32 = (1 << 22) - 1;
}

c_const! {
    pub const JOURNAL_ENTRY_BLOCKED_VAL: u32 = c::JOURNAL_ENTRY_OFFSET_MAX - 2;
}

c_const! {
    pub const JOURNAL_ENTRY_CLOSED_VAL: u32 = c::JOURNAL_ENTRY_OFFSET_MAX - 1;
}

c_const! {
    pub const JOURNAL_ENTRY_ERROR_VAL: u32 = c::JOURNAL_ENTRY_OFFSET_MAX;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_space {
    /* Units of 512 bytes sectors: */
    pub next_entry: core::ffi::c_uint, /* How big the next journal entry can be */
    pub total: core::ffi::c_uint,
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
    }
}

macro_rules! __journal_flags_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum journal_flags: u32 {
                $($acc)*
                $([<JOURNAL_ $n>],)*
            }
        }
    } };
}
JOURNAL_FLAGS!(__journal_flags_0 []);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct journal_bio {
    pub ca: *mut c::bch_dev,
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

/* journal's fastpath, a cacheline of its own: C's anonymous struct */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
#[c_align("SMP_CACHE_BYTES")]
pub struct journal_fastpath {
    #[c_anon("")] pub __align: [crate::types::CacheAligned; 0],
    pub reservations: c::journal_res_state,
    pub watermark: c::bch_watermark,
}

/* Embedded in struct bch_fs */
#[repr(C)]
#[derive(CStruct)]
#[c_align("SMP_CACHE_BYTES")]
pub struct journal {
    #[c_anon("")] pub __align: [crate::types::CacheAligned; 0],
    /* Fastpath stuff up front: */
    #[c_anon]
    pub fastpath: journal_fastpath,

    /*
     * We have to ratelimit at the journal for the allocator because the
     * btree write buffer allocates btree nodes after the journal -
     * otherwise we'd get HOL blocking and priority inversion:
     */
    pub watermark_open_buckets: u8,

    pub flags: core::ffi::c_ulong,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub stop_thread: *mut c::task_struct,

    /* Max size of current journal entry */
    pub cur_entry_u64s: core::ffi::c_uint,
    pub cur_entry_sectors: core::ffi::c_uint,

    /* Reserved space in journal entry to be used just prior to write */
    pub entry_u64s_reserved: core::ffi::c_uint,

    /*
     * 0, or -ENOSPC if waiting on journal reclaim, or -EROFS if
     * insufficient devices:
     */
    pub cur_entry_error: core::ffi::c_int,
    pub cur_entry_offset_if_blocked: core::ffi::c_uint,

    pub buf_size_want: core::ffi::c_uint,
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
     * Ring of slots indexed by seq & JOURNAL_STATE_BUF_MASK; only used by
     * the reservation fastpath. Updated in journal_entry_open() when a new
     * seq is assigned to the slot's state index.
     */
    pub ring: [c::journal_ringbuf; c::JOURNAL_STATE_BUF_NR as usize],

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

    pub free_buf: *mut core::ffi::c_void,
    pub free_buf_size: core::ffi::c_uint,

    pub lock: c::spinlock_t,

    /* if nonzero, we may not open a new journal entry: */
    pub blocked: core::ffi::c_uint,
    pub flushes_outstanding: core::ffi::c_uint,

    /* Used when waiting because the journal was full */
    pub async_wait: c::closure_waitlist,
    pub reclaim_flush_wait: c::closure_waitlist,

    pub write_work: c::delayed_work,
    pub wq: *mut c::workqueue_struct,
    pub discard_wq: *mut c::workqueue_struct,

    /* Sequence number of most recent journal entry (last entry in @pin) */
    pub seq: c::atomic64_t,

    pub seq_write_started: u64,
    /* seq, last_seq from the most recent journal entry successfully written */
    pub seq_ondisk: u64,
    pub flushed_seq_ondisk: u64,
    pub flushing_seq: c::atomic64_t,
    pub last_seq_ondisk: u64,
    pub err_seq: u64,
    pub last_empty_seq: u64,
    pub oldest_seq_found_ondisk: u64,

    /* Oldest journal seq that is safe to rewind to — discards of buckets
     * freed at >= this seq have not yet been issued */
    pub rewind_seq: u64,
    pub rewind_seq_ondisk: u64,

    /*
     * Rewind ranges: keys from journal entries with seq
     * in (to, from] use overwrite entries instead of
     * btree_keys entries.
     */
    #[c("DARRAY(struct journal_rewind_range) rewind_ranges")]
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
    #[c("FIFO_U64_IDX(struct journal_entry_pin_list) pin")]
    pub pin: Fifo<c::journal_entry_pin_list, u64>,
    pub pin_resize_lock: c::percpu_rw_semaphore,
    pub pin_resize_work: c::work_struct,

    /* Flushes under-replicated pins when a device comes back: */
    pub flush_degraded_work: c::work_struct,

    pub last_seq: u64,

    pub dirty_entry_bytes: usize,

    pub space: [c::journal_space; c::journal_space_nr as usize],

    pub replay_journal_seq: u64,
    pub replay_journal_seq_end: u64,

    pub wp: c::write_point,
    /*
     * Failure domain keys scratch for journal_write_alloc() - like
     * wp.stripe, protected by journal writes being allocated in order:
     */
    pub wp_domain_keys: [u64; c::BCH_SB_MEMBERS_MAX as usize],
    pub err_lock: c::spinlock_t,

    pub reclaim_lock: c::mutex,
    /*
     * Used for waiting until journal reclaim has freed up space in the
     * journal:
     */
    pub reclaim_wait: c::wait_queue_head_t,
    pub reclaim_thread: *mut c::task_struct,
    pub reclaim_kicked: bool,
    pub next_reclaim: core::ffi::c_ulong,
    pub nr_direct_reclaim: u64,
    pub nr_background_reclaim: u64,

    pub last_flushed: core::ffi::c_ulong,
    pub flush_in_progress: *mut c::journal_entry_pin,
    pub flush_in_progress_dropped: bool,
    pub pin_flush_wait: c::wait_queue_head_t,

    pub can_discard: bool,

    pub last_flush_write: core::ffi::c_ulong,

    pub write_start_time: u64,

    pub nr_flush_writes: u64,
    pub nr_noflush_writes: u64,
    pub entry_bytes_written: u64,

    pub flush_write_time: *mut c::bch2_time_stats,
    pub noflush_write_time: *mut c::bch2_time_stats,
    pub flush_seq_time: *mut c::bch2_time_stats,

    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
    pub res_map: c::lockdep_map,
}
c_default!(journal);

/*
 * Embedded in struct bch_dev. First three fields refer to the array of journal
 * buckets, in bch_sb.
 */
#[repr(C)]
#[derive(CStruct)]
pub struct journal_device {
    /*
     * For each journal bucket, contains the max sequence number of the
     * journal writes it contains - so we know when a bucket can be reused.
     */
    pub bucket_seq: *mut u64,

    pub sectors_free: core::ffi::c_uint,

    /*
     * discard_idx <= dirty_idx_ondisk <= dirty_idx <= cur_idx:
     */
    pub discard_idx: core::ffi::c_uint, /* Next bucket to discard */
    pub dirty_idx_ondisk: core::ffi::c_uint,
    pub dirty_idx: core::ffi::c_uint,
    pub cur_idx: core::ffi::c_uint, /* Journal bucket we're currently writing to */
    pub nr: core::ffi::c_uint,

    pub buckets: *mut u64,

    /*
     * Bioset for journal write bios. Journal writes allocate from this at
     * submit time and free on completion; the pool size bounds the
     * mempool reserve, not in-flight depth.
     */
    pub bio_set: c::bio_set,

    pub discard_lock: c::mutex,
    pub discard: c::work_struct,

    /* for bch_journal_read_device */
    pub read: c::closure,
    pub highest_seq_found: u64,
}
c_default!(journal_device);

/*
 * journal_entry_res - reserve space in every journal entry:
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_entry_res {
    pub u64s: core::ffi::c_uint,
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
