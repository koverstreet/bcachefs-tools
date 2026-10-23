// SPDX-License-Identifier: GPL-2.0

//! move.c's moving_context, for Rust. A move is asynchronous: move_extent()
//! returns once the read is issued, and the write, the index update and their
//! failures happen in completions. Rust has flush_all(), which waits for all
//! of it, ratelimit(), for enough of it, IoMark, for what was issued before a
//! point, and MoveStats, which completions count into; move.c's lists of IOs
//! stay in C.
//!
//! The context is pinned - it's on c->moving_context_list, its closure waited
//! on in place - and owns its transaction, trans(), which ratelimit(),
//! flush_all() and move_extent() unlock: keys borrowed from it are stale
//! after, as after any unlock.

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, BtreeTrans, TransAttempt};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::locking::Lock;
use crate::types::Atomic64;
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError};
use crate::util::ffi::{c_char, Opaque};
use crate::util::vstructs::Validated;
use core::ffi::{c_int, c_uint};
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_moving_ctxt_flush_all(ctxt: *mut c::moving_context);
    pub fn bch2_move_ratelimit(ctxt: *mut c::moving_context) -> c_int;
    pub fn bch2_move_extent(ctxt: *mut c::moving_context, bucket: *mut c::move_bucket,
                            opts: *mut c::bch_inode_opts, data_opts: *mut c::data_update_opts,
                            iter: *mut c::btree_iter, level: c_uint, k: c::bkey_s_c) -> c_int;
    pub fn bch2_move_stats_init(stats: *mut c::bch_move_stats, name: *const c_char);
}

/// What moves are counted into - by their completions, as well as by who
/// issued them: C's struct bch_move_stats, shared, its counters atomic.
#[repr(transparent)]
pub struct MoveStats(Opaque<c::bch_move_stats>);

impl MoveStats {
    pub fn from_c(stats: &c::bch_move_stats) -> &MoveStats {
        // SAFETY: repr(transparent) over an UnsafeCell of it: C writes through
        // it while it's borrowed, as it does through a &MoveStats
        unsafe { &*core::ptr::from_ref(stats).cast() }
    }

    fn raw(&self) -> *mut c::bch_move_stats {
        self.0.as_ptr()
    }

    fn counter(&self, f: impl FnOnce(*mut c::bch_move_stats) -> *mut Atomic64) -> &Atomic64 {
        // SAFETY: a field of the stats, which live as long as we do
        unsafe { &*f(self.raw()) }
    }

    /// Zeroed, and named @name for the moving contexts' listing.
    pub fn reset(&self, name: &core::ffi::CStr) {
        unsafe { bch2_move_stats_init(self.raw(), name.as_ptr().cast()) }
    }

    pub fn sectors_seen(&self) -> u64 {
        self.counter(|s| unsafe { &raw mut (*s).sectors_seen }).load(Relaxed) as u64
    }

    pub fn add_sectors_seen(&self, sectors: u64) {
        self.counter(|s| unsafe { &raw mut (*s).sectors_seen }).fetch_add(sectors as i64, Relaxed);
    }

    pub fn sectors_moved(&self) -> u64 {
        self.counter(|s| unsafe { &raw mut (*s).sectors_moved }).load(Relaxed) as u64
    }

    /// Where whoever's moving through it is, for the listing: read racily, as
    /// C's are.
    pub fn set_pos(&self, pos: c::bbpos) {
        unsafe { (&raw mut (*self.raw()).at.logical.pos).write(pos) }
    }
}

/// A point in a context's IO, for io_done(): what was issued before it, and what
/// after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoMark(u64);

pub struct MovingCtxt<'a> {
    ctxt: NonNull<c::moving_context>,
    life: PhantomData<(&'a Fs, &'a MoveStats)>,
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
                c::bch2_moving_ctxt_init(slot, fs.as_ptr(), core::ptr::null_mut(), stats.raw(),
                                         wp, wait_on_copygc);
                Ok::<(), AllocError>(())
            })
        };
        let ctxt = alloc_pinned(init)?;
        Ok(MovingCtxt { ctxt: NonNull::new(ctxt).expect("alloc_pinned() returns its allocation"), life: PhantomData })
    }

    /// For C that takes the context: stripe repair, which moves through it.
    pub(crate) fn raw(&self) -> *mut c::moving_context {
        self.ctxt.as_ptr()
    }

    pub fn trans(&self) -> &BtreeTrans<'a> {
        // SAFETY: the context's, live until it's dropped
        unsafe { BtreeTrans::borrow_raw((*self.raw()).trans) }
    }

    /// Count moves from here on into @stats - as the listing has it, which
    /// reads the pointer racily, as it does in C.
    pub fn set_stats(&self, stats: &'a MoveStats) {
        unsafe { (*self.raw()).stats = stats.raw() }
    }

    pub fn stats(&self) -> &MoveStats {
        // SAFETY: set from a &'a MoveStats, in new() or set_stats()
        unsafe { &*(*self.raw()).stats.cast() }
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

    /// Repair stripe @s, at @iter, if it's degraded - a block on a device
    /// that's missing, evacuating, or lost its durability: rebuilt at its
    /// width if there are devices for it, narrowed if not, rebuilt around
    /// what's left if it can't be reconstructed. bch2_stripe_repair().
    ///
    /// @iter must be intent locked - the stripe's held against its trigger
    /// until it's open - and repair drops the locks to read and write, and
    /// runs attempts of its own: so @s is a copy, not a key in the btree, and
    /// if it did, the attempt ends in transaction_restart_nested
    /// (relock_or_nested()). stripe_needs_block_evacuate: a block must be
    /// evacuated first.
    pub fn stripe_repair(&self, t: &TransAttempt<'_, 'a>, iter: &mut BtreeIter<'_>, s: BkeySC<'_, c::Stripe<Validated>>)
        -> Result<(), BchError>
    {
        // SAFETY: in the attempt; @s the caller's copy, which the unlocks
        // inside don't touch
        ret_to_result_void(unsafe { c::bch2_stripe_repair(self.raw(), iter.raw_mut(), s.to_c_whole()) })?;
        t.relock_or_nested()
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
            c::bch2_moving_ctxt_exit(self.raw());
            free_pinned(self.raw());
        }
    }
}
