// SPDX-License-Identifier: GPL-2.0

//! Filesystem RO/RW
//!
//! For startup/shutdown of RW stuff, the dependencies are:
//!
//! - foreground writes depend on copygc and rebalance (to free up space)
//!
//! - copygc and rebalance depend on mark and sweep gc (they actually probably
//!   don't because they either reserve ahead of time or don't block if
//!   allocations fail, but allocations can require mark and sweep gc to run
//!   because of generation number wraparound)
//!
//! - all of the above depends on the allocator threads
//!
//! - allocator depends on the journal (when it rewrites prios and gens)

use core::sync::atomic::Ordering::Relaxed;

use crate::init::passes::RecoveryPass;
use crate::accounting::verify_accounting_clean;
use crate::alloc::capacity::recalc_capacity;
use crate::btree::cache::bch2_fs_btree_init_rw;
use crate::btree::interior::{bch2_btree_interior_updates_flush, bch2_do_pending_node_rewrites};
use crate::btree::write::bch2_btree_flush_all_writes;
use crate::btree::write_buffer::{bch2_btree_write_buffer_flush_going_ro, bch2_btree_write_buffer_start,
                                 bch2_btree_write_buffer_stop};
use crate::c;
use crate::c::bch_fs_flags as Flag;
use crate::c::journal_flags as Journal;
use crate::c::JournalGuard;
use crate::data::ec::head::stripe_heads_stop;
use crate::errcode::{bch_errcode, ret_to_result_void as ret_to_result, BchError};
use crate::format::{MemberState, SbFeature};
use crate::fs::{Fs, StateLockWrite};
use crate::sb::members::{BtreeBitmapGcSchedule, SbMembersMut};
use crate::util::futex::EventCount;
use crate::util::ffi::Opaque;
use crate::util::kernel::WRITE;
use crate::util::rcu::read_lock;
use crate::util::sleep::Sleep;
use crate::util::workqueue::Queue;
use crate::util::Printbuf;
use crate::{bch_err, bch_info, bch_verbose, prt, warn_on};
use cstruct_macros::c_export;

static READ_ONLY_EVENTS: EventCount = EventCount::new();

impl Fs {
    fn stop_rw(&self) {
        let p = self.as_ptr();
        let mut clean_passes = 0;
        let mut seq = 0;

        self.maybe_schedule_btree_bitmap_gc.cancel_sync();
        stripe_heads_stop(self, None);
        self.open_buckets_stop(None, true);
        self.copygc_stop();
        unsafe { bch2_btree_write_buffer_stop(p) };
        self.ec_flush();

        bch_verbose!(self, "flushing journal and stopping allocators, journal seq {}",
                     self.journal_cur_seq());

        while clean_passes < 2 {
            clean_passes += 1;

            self.do_discards_going_ro();

            let progress = unsafe {
                bch2_btree_write_buffer_flush_going_ro(p) ||
                c::bch2_btree_key_cache_flush_going_ro(p) != 0 ||
                bch2_btree_interior_updates_flush(p) ||
                self.journal_flush_all_pins() ||
                bch2_btree_flush_all_writes(p)
            };
            if progress || seq != self.journal_cur_seq() {
                seq = self.journal_cur_seq();
                clean_passes = 0;
            }
        }

        /*
         * On a dead journal the clean-shutdown loop above can't make dirty
         * btree nodes clean: journal_flush_all_pins doesn't push them out,
         * and __bch2_btree_node_write skips submission for journal_error, so
         * BTREE_NODE_dirty stays set on every node that hadn't been written
         * yet. The loop terminates (everything no-ops) but live[].dirty is
         * still populated. Force-cancel: drop dirty bits + transition DIRTY →
         * CLEAN, then drain any interior updates btree_node_write_update_key
         * may have kicked off before the journal died.
         */
        if self.journal.halted() {
            self.btree_cancel_all_writes();
            unsafe { bch2_btree_interior_updates_flush(p) };
        } else {
            crate::journal::buckets::drop_retired_going_ro(self);
        }

        bch_verbose!(self, "flushing journal and stopping allocators complete, journal seq {}",
                     self.journal_cur_seq());

        if self.journal.flags.get(Journal::replay_done) && !self.flags.get(Flag::emergency_ro) {
            self.flags.set(Flag::clean_shutdown, true);
        }

        self.journal.stop();

        bch_info!(self, "{}clean shutdown complete, journal seq {}",
                  if self.flags.get(Flag::clean_shutdown) { "" } else { "un" },
                  self.journal.lock.seq_ondisk.get());

        /*
         * After stopping journal:
         */
        for ca in self.members() {
            ca.io_ref_stop(WRITE);
            self.dev_allocator_remove(&ca);
        }
    }

    fn emergency_read_only_to(&self, out: &mut Printbuf, journal: Option<&mut JournalGuard<'_>>) -> bool {
        let ret = !self.flags.swap(Flag::emergency_ro, true);

        match journal {
            Some(g) => self.journal.halt_locked(g),
            None => self.journal.halt(),
        }
        self.read_only_work.queue(Queue::system_long());
        READ_ONLY_EVENTS.wake();

        /*
         * Wake threads parked in __bch2_wait_on_allocator: going-RO won't
         * complete while they hold open closures on freelist_wait, and they
         * won't otherwise notice the fs is shutting down.
         */
        self.alloc_wake_all();

        if ret {
            prt!(out, "emergency read only at seq {}\n", self.journal_cur_seq());
            out.set_suppressed(false);
        }
        ret
    }

    /// True if this call is the one that went read-only - then @out is
    /// printed, not suppressed
    pub fn emergency_read_only(&self, out: &mut Printbuf) -> bool {
        self.emergency_read_only_to(out, None)
    }

    pub fn emergency_read_only_locked(&self, out: &mut Printbuf, g: &mut JournalGuard<'_>) -> bool {
        self.emergency_read_only_to(out, Some(g))
    }

}

impl StateLockWrite<'_> {
    pub fn read_only(&self) {
        let fs = self.fs();

        if !fs.flags.get(Flag::rw) {
            fs.journal.reclaim_stop();
            return;
        }

        assert!(!fs.flags.get(Flag::write_disable_complete));

        bch_verbose!(fs, "going read-only");

        /*
         * Block new foreground-end write operations from starting - any new
         * writes will return -EROFS. Set before stopping reconcile so the
         * reconcile kthread's child workers (which check this flag to break
         * out) see it while bch2_reconcile_stop() is blocked waiting on them.
         */
        fs.flags.set(Flag::going_ro, true);

        /*
         * Stop background kthreads that issue writes (reconcile, etc.)
         * before disabling c->writes; otherwise they can dispatch
         * data_update operations that fail with erofs_no_writes once
         * c->writes is stopped, and the failure-rate threshold trips.
         */
        fs.reconcile_stop();

        fs.writes.stop_async();

        /*
         * If we're not doing an emergency shutdown, we want to wait on
         * outstanding writes to complete so they don't see spurious errors due
         * to shutting down the allocator:
         *
         * If we are doing an emergency shutdown outstanding writes may
         * hang until we shutdown the allocator so we don't want to wait
         * on outstanding writes before shutting everything down - but
         * we do need to wait on them before returning and signalling
         * that going RO is complete:
         */
        let writes_stopped = || fs.flags.get(Flag::write_disable_complete);
        let Ok(()) = READ_ONLY_EVENTS.wait_until(Sleep::UNINTERRUPTIBLE, || {
            (writes_stopped() || fs.flags.get(Flag::emergency_ro)).then_some(())
        });

        let writes_disabled = writes_stopped();
        if writes_disabled {
            bch_verbose!(fs, "finished waiting for writes to stop");
        }

        fs.stop_rw();

        let Ok(()) = READ_ONLY_EVENTS.wait_until(Sleep::UNINTERRUPTIBLE, || writes_stopped().then_some(()));

        if !writes_disabled {
            bch_verbose!(fs, "finished waiting for writes to stop");
        }

        fs.flags.set(Flag::write_disable_complete, false);
        fs.flags.set(Flag::going_ro, false);
        fs.flags.set(Flag::rw, false);

        if !fs.journal.halted() &&
           !fs.flags.get(Flag::error) &&
           !fs.flags.get(Flag::emergency_ro) &&
           fs.flags.get(Flag::started) &&
           fs.flags.get(Flag::clean_shutdown) &&
           fs.recovery.runner.pass_done.get() >= RecoveryPass::journal_replay {
            assert_eq!(fs.journal.lock.lock().last_empty_seq, fs.journal_cur_seq());
            assert!(fs.sb.clean.get() != 0);
            assert!(fs.btree.cache.lock.nr_dirty() == 0);
            assert_eq!(fs.btree.key_cache.nr_dirty.load(Relaxed), 0);
            for wb in &fs.btree.write_buffer {
                assert_eq!(wb.inc.keys.nr, 0);
                assert_eq!(wb.flushing.keys.nr, 0);
            }
            fs.verify_replicas_refs_clean();
            verify_accounting_clean(fs);
        } else {
            /* Make sure error counts/counters are persisted */
            fs.write_super(&mut fs.sb_lock());

            bch_verbose!(fs, "done going read-only, filesystem not clean");
        }
    }

    pub fn init_rw(&self) -> Result<(), BchError> {
        let fs = self.fs();
        let p = fs.as_ptr();

        if fs.flags.get(Flag::rw_init_done) {
            return Ok(());
        }

        fs.init_late(self).map_err(|e| fs.err(e))?;
        unsafe {
            ret_to_result(bch2_fs_btree_init_rw(p))?;
            (*p).journal.init_rw()?;
            (*p).journal.reclaim_start()?;
            ret_to_result(bch2_btree_write_buffer_start(p))?;
        }
        fs.copygc_start()?;
        fs.reconcile_start()?;

        fs.flags.set(Flag::rw_init_done, true);
        Ok(())
    }

    pub(crate) fn start_rw(&self) -> Result<(), BchError> {
        let fs = self.fs();
        let p = fs.as_ptr();

        assert!(fs.flags.get(Flag::may_go_rw));

        if warn_on!(fs.feature(SbFeature::no_alloc_info)) {
            return fs.throw(bch_errcode::BCH_ERR_erofs_no_alloc_info);
        }

        assert!(fs.flags.get(Flag::may_upgrade_downgrade));

        if fs.flags.get(Flag::initial_gc_unfixed) {
            bch_err!(fs, "cannot go rw, unfixed btree errors");
            return fs.throw(bch_errcode::BCH_ERR_erofs_unfixed_errors);
        }

        if fs.feature(SbFeature::small_image) {
            bch_err!(fs, "cannot go rw, filesystem is an unresized image file");
            return fs.throw(bch_errcode::BCH_ERR_erofs_filesystem_full);
        }

        if fs.feature(SbFeature::no_default_sb) {
            bch_err!(fs, "cannot go rw, run bcachefs migrate-superblock first");
            return fs.throw(bch_errcode::BCH_ERR_erofs_sb_not_migrated);
        }

        if fs.flags.get(Flag::rw) {
            return Ok(());
        }

        bch_info!(fs, "going read-write");

        self.init_rw()?;
        {
            let mut sb = fs.sb_lock();
            fs.sb_resize(&mut sb).members_v2_init()
                .ok_or_else(|| fs.err(bch_errcode::BCH_ERR_ENOSPC_sb_members_v2))?;
        }

        fs.flags.set(Flag::clean_shutdown, false);

        {
            let rcu = read_lock();
            for ca in fs.online_devs_rcu(&rcu) {
                if ca.mi.get().state == MemberState::rw {
                    self.dev_allocator_add(ca);
                    ca.io_ref[WRITE].start();
                }
            }
        }

        recalc_capacity(fs);

        /*
         * First journal write must be a flush write: after a clean shutdown we
         * don't read the journal, so the first journal write may end up
         * overwriting whatever was there previously, and there must always be
         * at least one non-flush write in the journal or recovery will fail:
         */
        {
            let mut g = fs.journal.lock.lock();
            fs.journal.flags.set(Journal::need_flush_write, true);
            fs.journal.flags.set(Journal::running, true);
            #[cfg(CONFIG_BCACHEFS_DEBUG)]
            fs.journal.stop_thread.store(core::ptr::null_mut(), Relaxed);
            fs.journal.space_available(&mut g);
        }

        /*
         * Don't jump to our error path, and call bch2_fs_read_only(), unless we
         * successfully marked the filesystem dirty
         */

        fs.flags.set(Flag::rw, true);
        fs.flags.set(Flag::was_rw, true);

        if fs.flags.swap(Flag::sb_dirty, false) {
            fs.write_super(&mut fs.sb_lock());
        }

        fs.writes.start();

        let ret = (|| {
            fs.journal.reclaim_start()?;
            ret_to_result(unsafe { bch2_btree_write_buffer_start(p) })?;
            fs.copygc_start()?;
            if fs.opts().read_only.get() == 0 {
                fs.reconcile_start()?;
            }
            Ok(())
        })();
        if ret.is_err() {
            self.read_only();
            return ret;
        }

        /* copygc was started before recovery, and waits for BCH_FS_rw: */
        fs.copygc_wakeup();
        fs.do_discards_async();
        fs.do_invalidates();
        fs.do_stripe_deletes();
        unsafe { bch2_do_pending_node_rewrites(p) };
        crate::data::scrub_journal::do_repairs(fs);
        BtreeBitmapGcSchedule::run(fs);
        Ok(())
    }

    pub fn read_write(&self) -> Result<(), BchError> {
        let fs = self.fs();
        let opts = fs.opts();
        let pass_last = u32::from(opts.recovery_pass_last.get());

        if pass_last != 0 && pass_last < RecoveryPass::journal_replay as u32 {
            return fs.throw(bch_errcode::BCH_ERR_erofs_norecovery);
        }

        if opts.nochanges.get() != 0 {
            return fs.throw(bch_errcode::BCH_ERR_erofs_nochanges);
        }

        if fs.feature(SbFeature::no_alloc_info) {
            return fs.throw(bch_errcode::BCH_ERR_erofs_no_alloc_info);
        }

        self.start_rw()
    }
}

pub(crate) fn read_only_work(fs: &Fs) {
    fs.state_lock_write().read_only();
}

pub(crate) unsafe extern "C" fn writes_disabled(writes: *mut c::enumerated_ref) {
    let fs = unsafe { &*writes.byte_sub(core::mem::offset_of!(Fs, writes)).cast::<Fs>() };
    fs.flags.set(Flag::write_disable_complete, true);
    READ_ONLY_EVENTS.wake();
}

#[c_export]
pub fn bch2_fs_emergency_read_only(c: &Opaque<Fs>, out: &mut Printbuf) -> bool {
    Fs::from_c(c).emergency_read_only(out)
}
