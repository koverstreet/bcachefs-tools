// SPDX-License-Identifier: GPL-2.0

//! move.c's moving_context, for Rust. A move is asynchronous: move_extent()
//! returns once the read is issued, and the write, the index update and their
//! failures happen in completions. Rust has flush_all(), which waits for all
//! of it, ratelimit(), for enough of it, IoMark, for what was issued before a
//! point, wait_until(), for a condition completions change, and MoveStats,
//! which completions count into; move.c's lists of IOs stay in C.
//!
//! C's completions wake the context's wait queue, which C's waits sleep on;
//! Rust's sleep on its events, an EventCount, which C wakes beside it -
//! bch2_moving_ctxt_wake() - until move.c's waits are Rust and the queue goes.
//!
//! The context is pinned - it's on c->moving_context_list, its closure waited
//! on in place - and owns its transaction, trans(), which ratelimit(),
//! flush_all() and move_extent() unlock: keys borrowed from it are stale
//! after, as after any unlock. MovingCtxt owns one; moving through it is
//! Moving's, a borrow - of a MovingCtxt, or of a context C hands Rust. A write
//! that outlives the borrow, counted in the context, is a MovingWrite.

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, BtreeTrans, TransAttempt};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::locking::Lock;
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError};
use crate::util::ffi::Opaque;
use crate::util::sleep::{wake_up, Sleep};
use core::ffi::{c_int, c_uint, c_void};
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{c_export, c_extern};

c_extern! {
    pub fn bch2_moving_ctxt_flush_all(ctxt: *mut c::moving_context);
    pub fn bch2_move_ratelimit(ctxt: *mut c::moving_context) -> c_int;
    pub fn bch2_move_extent(ctxt: *mut c::moving_context, bucket: *mut c::move_bucket,
                            opts: *mut c::bch_inode_opts, data_opts: *mut c::data_update_opts,
                            iter: *mut c::btree_iter, level: c_uint, k: c::bkey_s_c) -> c_int;
    pub fn bch2_moving_ctxt_do_pending_writes(ctxt: *mut c::moving_context);
    pub fn bch2_moving_ctxt_next_pending_write(ctxt: *mut c::moving_context) -> *mut c::data_update;
    pub fn bch2_evacuate_bucket(ctxt: *mut c::moving_context, bucket_in_flight: *mut c::move_bucket,
                                bucket: c::bpos, gen: c_int, data_opts: c::data_update_opts) -> c_int;
    pub fn bch2_evacuate_data(ctxt: *mut c::moving_context, dev: c_uint, start: u64, end: u64) -> c_int;
    pub fn bch2_evacuate_ec_orphan(ctxt: *mut c::moving_context, ec_idx: u64, ec_block: c_uint,
                                   start: u64, end: u64) -> c_int;
}

/// Wake what waits on @ctxt in Rust - its events, as wake_up(&ctxt->wait)
/// wakes C's waits: called alongside it, from completions.
///
/// # Safety
/// @ctxt is a live context.
#[c_export]
pub unsafe fn bch2_moving_ctxt_wake(ctxt: *mut c::moving_context) {
    // SAFETY: the caller's; only the count is borrowed, which is atomic
    unsafe { (*ctxt).events.wake() }
}

pub use c::MoveStats;

impl MoveStats {
    /// Zeroed, and named @name for the moving contexts' listing, as
    /// bch2_move_stats_init() - field by field, as it may be read meanwhile.
    pub fn reset(&self, name: &core::ffi::CStr) {
        let name = name.to_bytes();
        let len = name.len().min(self.name.len() - 1);
        for (i, b) in self.name.iter().enumerate() {
            b.set(if i < len { name[i] } else { 0 });
        }
        self.phys.set(false);
        self.ret.set(c::bch_ioctl_data_event_ret(0));
        self.at.data_type.set(c::bch_data_type::BCH_DATA_user);
        self.at.pos.set(c::bbpos::default());
        self.at.dev.set(0);
        self.at.offset.set(0);

        for n in [&self.keys_moved, &self.keys_raced, &self.sectors_seen, &self.sectors_moved,
                  &self.sectors_raced, &self.sectors_error_corrected, &self.sectors_error_uncorrected] {
            n.store(0, Relaxed);
        }
        self.devs_error_uncorrected.store(&Default::default());
    }

    pub fn sectors_seen(&self) -> u64 {
        self.sectors_seen.load(Relaxed) as u64
    }

    pub fn add_sectors_seen(&self, sectors: u64) {
        self.sectors_seen.fetch_add(sectors as i64, Relaxed);
    }

    pub fn sectors_moved(&self) -> u64 {
        self.sectors_moved.load(Relaxed) as u64
    }

    /// Where whoever's moving through it is, for the listing.
    pub fn set_pos(&self, pos: c::bbpos) {
        self.at.pos.set(pos);
    }
}

/// A point in a context's IO, for io_done(): what was issued before it, and what
/// after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoMark(u64);

/// Freed - once what's in flight is done - when it's dropped.
pub struct MovingCtxt<'a> {
    ctxt: NonNull<c::moving_context>,
    life: PhantomData<(&'a Fs, &'a MoveStats)>,
}

#[repr(transparent)]
pub struct Moving<'a>(Opaque<c::moving_context>, PhantomData<(&'a Fs, &'a MoveStats)>);

/// Counted in write_sectors and write_ios, which ratelimit() throttles on,
/// and a ref on the context's closure, which flush_all() and the context's
/// teardown wait for. Dropped, it's done: the counts given back, the
/// context's waiters woken, the ref put.
#[must_use = "the write is done as soon as it's dropped"]
pub struct MovingWrite {
    ctxt:    NonNull<c::moving_context>,
    sectors: u32,
}

// SAFETY: it touches only the context's counters, which are atomics, its wait
// queue and its closure: any thread's. Its closure ref keeps the context.
unsafe impl Send for MovingWrite {}

impl Drop for MovingWrite {
    fn drop(&mut self) {
        let ctxt = self.ctxt.as_ptr();
        // SAFETY: our closure ref keeps the context, until its put - the last
        // touch
        unsafe {
            (*ctxt).write_sectors.fetch_sub(self.sectors as i32, Relaxed);
            (*ctxt).write_ios.fetch_sub(1, Relaxed);
            wake_up(&(*ctxt).wait);
            bch2_moving_ctxt_wake(ctxt);
            c::closure_put((*ctxt).cl.as_ptr());
        }
    }
}

impl<'a> MovingCtxt<'a> {
    /// Moves counted into @stats, written to @wp; with @wait_on_copygc,
    /// ratelimit() waits while copygc runs.
    pub fn new(fs: &'a Fs, stats: &'a MoveStats, wp: c::write_point_specifier, wait_on_copygc: bool)
        -> Result<MovingCtxt<'a>, AllocError>
    {
        // SAFETY: bch2_moving_ctxt_init() initializes all of it, in place
        let init = unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut c::moving_context| {
                c::bch2_moving_ctxt_init(slot, fs.as_ptr(),
                                         core::ptr::from_ref(stats).cast_mut(),
                                         wp, wait_on_copygc);
                Ok::<(), AllocError>(())
            })
        };
        let ctxt = alloc_pinned(init)?;
        Ok(MovingCtxt { ctxt: NonNull::new(ctxt).expect("alloc_pinned() returns its allocation"), life: PhantomData })
    }
}

impl<'a> core::ops::Deref for MovingCtxt<'a> {
    type Target = Moving<'a>;

    fn deref(&self) -> &Moving<'a> {
        // SAFETY: ours, live and pinned until we're dropped
        unsafe { Moving::from_raw(self.ctxt.as_ptr()) }
    }
}

impl<'a> Moving<'a> {
    /// # Safety
    /// @ctxt is initialized, and outlives the borrow, and its filesystem and
    /// stats live for 'a.
    pub unsafe fn from_raw<'r>(ctxt: *mut c::moving_context) -> &'r Self {
        // SAFETY: the caller's; repr(transparent) over it
        unsafe { &*ctxt.cast() }
    }

    /// For C that takes the context: stripe repair, which moves through it.
    pub(crate) fn raw(&self) -> *mut c::moving_context {
        self.0.as_ptr()
    }

    /// A write of @sectors, in flight in this context until the MovingWrite's
    /// dropped.
    pub fn write_start(&self, sectors: u32) -> MovingWrite {
        let ctxt = self.raw();
        // SAFETY: borrowed, live; the counters atomics
        unsafe {
            (*ctxt).write_sectors.fetch_add(sectors as i32, Relaxed);
            (*ctxt).write_ios.fetch_add(1, Relaxed);
            c::closure_get((*ctxt).cl.as_ptr());
        }
        MovingWrite { ctxt: NonNull::new(ctxt).expect("a context"), sectors }
    }

    pub fn trans(&self) -> &BtreeTrans<'a> {
        // SAFETY: the context's, live as long as it is
        unsafe { BtreeTrans::borrow_raw((*self.raw()).trans) }
    }

    /// Count moves from here on into @stats - as the listing has it, which
    /// reads the pointer racily, as it does in C.
    pub fn set_stats(&self, stats: &'a MoveStats) {
        unsafe { (*self.raw()).stats = core::ptr::from_ref(stats).cast_mut() }
    }

    pub fn stats(&self) -> &MoveStats {
        // SAFETY: set from a &'a MoveStats, in new() or set_stats()
        unsafe { &*(*self.raw()).stats }
    }

    /// Wait until fewer than the filesystem's limits are in flight - and, if
    /// it waits on copygc, until copygc isn't running: errors if the
    /// filesystem's going read-only, or the thread's stopping.
    pub fn ratelimit(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_move_ratelimit(self.raw()) })
    }

    pub fn flush_all(&self) {
        unsafe { bch2_moving_ctxt_flush_all(self.raw()) }
    }

    /// Until @cond, as move_ctxt_wait_event(): writing what's been read as it
    /// completes, and asleep with the transaction unlocked. @cond runs in the
    /// sleep's task state too, so it mustn't sleep.
    pub fn wait_until<T>(&self, mut cond: impl FnMut() -> Option<T>) -> T {
        // SAFETY: the context's, live until it's dropped; an atomic
        let events = unsafe { &(*self.raw()).events };
        loop {
            unsafe { bch2_moving_ctxt_do_pending_writes(self.raw()) };
            if let Some(v) = cond() {
                return v;
            }
            self.trans().unlock_long();

            // A read completed - its write to issue - or @cond
            let Ok(v) = events.wait_until(Sleep::UNINTERRUPTIBLE.freezable(), || {
                if unsafe { !bch2_moving_ctxt_next_pending_write(self.raw()).is_null() } {
                    Some(None)
                } else {
                    cond().map(Some)
                }
            });
            if let Some(v) = v {
                return v;
            }
        }
    }

    /// Start moving @k, at @iter's position in the context's transaction, as
    /// @data_opts say - returning once the read is issued. @k is copied
    /// before the transaction is unlocked to issue it: past this the attempt
    /// has no commit left to make. The read retries itself - restarts in its
    /// erasure coded fallback - which aren't the attempt's.
    pub fn move_extent(
        &self,
        t:         &TransAttempt<'_, 'a>,
        opts:      &mut c::bch_inode_opts,
        data_opts: &mut c::data_update_opts,
        iter:      &mut BtreeIter<'_>,
        level:     u32,
        k:         BkeySC<'_>,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            bch2_move_extent(self.raw(), core::ptr::null_mut(), opts, data_opts, iter.raw_mut(), level, k.into())
        };
        assert!(ret <= 0, "bch2_move_extent() returned {ret}");
        t.restarts_handled();
        t.result(ret)
    }

    /// Start moving what @pred picks of @btree at @level, @start to @end -
    /// returning once the moves are issued.
    ///
    /// # Safety
    /// @arg is what @pred takes.
    pub unsafe fn move_data_btree(&self, start: c::bpos, end: c::bpos, pred: c::move_pred_fn, arg: *mut c_void,
                                  btree: c::btree_id, level: u32) -> Result<(), BchError> {
        // SAFETY: the caller's
        ret_to_result_void(unsafe { c::bch2_move_data_btree(self.raw(), start, end, pred, arg, btree, level) })
    }

    /// Start moving the data in sectors @start..@end of device @dev off it,
    /// found by backpointer - to the extents' background target - returning
    /// once the moves are issued, ratelimit()ed, not done.
    /// Runs attempts of its own in the context's transaction.
    pub fn evacuate_data(&self, dev: u32, start: u64, end: u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_evacuate_data(self.raw(), dev, start, end) })
    }

    /// evacuate_data(), for block @block of stripe @idx, on a device that's
    /// been removed: its extents found by their stripe_backpointers, the
    /// pointers moved those that point to the block.
    pub fn evacuate_ec_orphan(&self, idx: u64, block: u32, start: u64, end: u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_evacuate_ec_orphan(self.raw(), idx, block, start, end) })
    }

    /// Start moving everything off @key's bucket, while it's that
    /// generation, as @data_opts say: its backpointers walked, each extent's
    /// move issued - returning once the reads are. Each move holds a count on
    /// @bucket_in_flight until its update is done.
    ///
    /// # Safety
    /// @bucket_in_flight stays valid, where it is, until its count is back
    /// to what it was.
    pub unsafe fn evacuate_bucket(&self, bucket_in_flight: NonNull<c::move_bucket>, key: c::move_bucket_key,
                                  data_opts: c::data_update_opts) -> Result<(), BchError> {
        ret_to_result_void(unsafe {
            bch2_evacuate_bucket(self.raw(), bucket_in_flight.as_ptr(), key.bucket, key.generation as c_int,
                                 data_opts)
        })
    }

    pub fn io_mark(&self) -> IoMark {
        // SAFETY: only issuing - this context's owner - moves it on
        IoMark(unsafe { (*self.raw()).io_seq })
    }

    /// Whether everything issued before @mark has completed.
    pub fn io_done(&self, mark: IoMark) -> bool {
        // SAFETY: the context's lock, which completions take to move
        // io_seq_done on
        let lock = Lock::from_c(unsafe { &(*self.raw()).lock });
        let _guard = lock.lock();
        unsafe { (&raw const (*self.raw()).io_seq_done).read() >= mark.0 }
    }
}

impl Drop for MovingCtxt<'_> {
    fn drop(&mut self) {
        // SAFETY: ours, from new(); bch2_moving_ctxt_exit() waits for what's
        // in flight before taking it off the list and putting the transaction
        unsafe {
            c::bch2_moving_ctxt_exit(self.ctxt.as_ptr());
            free_pinned(self.ctxt.as_ptr());
        }
    }
}
