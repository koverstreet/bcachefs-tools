// SPDX-License-Identifier: GPL-2.0

use core::ffi::{c_int, CStr};
use core::marker::PhantomData;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::Ordering::Relaxed;

use cstruct_macros::c_export;
use zerocopy::IntoBytes;

use crate::accounting::Accounting;
use crate::alloc::accounting_mem::{self, mem_add, Delta, MarkRead};
use crate::alloc::capacity::account_disk_usage_change;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_validate, BkeyI, BkeySC, ValidateFlags, BKEY_U64S};
use crate::btree::cache::BTREE_WRITE_IO_LIMIT;
use crate::btree::interior::{merge, split_leaf};
use crate::btree::iter::{is_restart, BtreeTrans, CommitFlags, CommitOpts, TransAttempt};
use crate::btree::journal_overlay;
use crate::btree::key_cache::{bch2_btree_insert_key_cached, bch2_btree_key_cache_drop};
use crate::btree::node::BtreeNode;
use crate::btree::path::{NodePtr, PathIdx, Paths, PathsUpdates};
use crate::btree::trigger::{bch2_verify_update_old_key, run_mem_triggers, run_trans_triggers};
use crate::btree::types::BtreeNodeType;
use crate::c;
use crate::c::journal_flags as Journal;
use crate::cstructs::c as cs;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, ret_to_c, ret_to_result_void as ret_to_result, BchError};
use crate::format::{BkeyType, Btree, Bversion};
use crate::init::error::BkeyValidate;
use crate::journal::entries::NoRoom;
use crate::journal::validate::journal_entry_validate;
use crate::locking::MemallocFlags;
use crate::util::alloc::kmalloc;
use crate::util::backtrace::{prt_symbol, prt_task_backtrace};
use crate::util::kernel::{likely, prefetch, unlikely};
use crate::util::log::LogMsg;
use crate::util::refcount::TryRefcount;
use crate::util::sched;
use crate::util::time_stats::TimeStats;
use crate::util::vstructs::{plain_bytes, EntryWalk, Unsized, Unvalidated, Validated};
use crate::{bch_err, fs_fatal_error, fs_inconsistent, prt};

const JSET_ENTRY_LOG_U64S: usize = 4;

fn jset_u64s(u64s: usize) -> usize {
    u64s + c::JSET_KEYS_U64s as usize
}

fn watermark(flags: CommitOpts) -> u32 {
    flags.to_c().bits() & c::BCH_WATERMARK_MASK
}

fn has(flags: CommitOpts, f: CommitFlags) -> bool {
    flags.to_c().bits() & f.bits() != 0
}

fn insert_l(paths: &Paths<'_>, i: &c::btree_insert_entry) -> NodePtr<cs::btree_bkey_cached_common> {
    paths[i.path].node(i.level().into())
}

fn same_leaf_as_prev(paths: &Paths<'_>, u: &[c::btree_insert_entry], idx: usize) -> bool {
    idx != 0 && insert_l(paths, &u[idx]) == insert_l(paths, &u[idx - 1])
}

fn same_leaf_as_next(paths: &Paths<'_>, u: &[c::btree_insert_entry], idx: usize) -> bool {
    idx + 1 < u.len() && insert_l(paths, &u[idx]) == insert_l(paths, &u[idx + 1])
}

/// # Safety
/// @i's path holds its node locked - for as long as the reference is used.
unsafe fn leaf<'a>(paths: &Paths<'_>, i: &c::btree_insert_entry) -> &'a BtreeNode {
    let b = paths[i.path].locked_btree_node(i.level().into()).expect("an update's path has its node locked");
    unsafe { &*ptr::from_ref(b.node()) }
}

fn cached_entry(paths: &Paths<'_>, idx: PathIdx) -> *mut cs::bkey_cached {
    paths[idx].node(0).as_ptr().cast()
}

/// The transaction's journal entries are its own, made by its updates: trusted,
/// as C trusts them.
fn for_each_journal_entry(t: &TransAttempt<'_, '_>,
                          f: impl FnMut(&c::jset_entry<Validated>) -> Result<(), BchError>)
    -> Result<(), BchError>
{
    t.trans().journal_entries.get().iter(t).try_for_each(f)
}

fn is_keys_entry(e: &c::jset_entry<Validated>) -> bool {
    let ty = c::bch_jset_entry_type(e.type_.into());
    ty == c::bch_jset_entry_type::btree_keys || ty == c::bch_jset_entry_type::write_buffer_keys
}

fn for_each_entry_key(e: &c::jset_entry<Validated>,
                      mut f: impl FnMut(&BkeyI) -> Result<(), BchError>) -> Result<(), BchError> {
    e.entries().try_for_each(|k| f(k.key()))
}

impl BtreeTrans<'_> {
    #[inline(never)]
    fn lock_write_fail(&self, pu: &mut PathsUpdates<'_, '_>, end: usize) {
        let PathsUpdates { paths, updates } = pu;
        for (idx, i) in updates[..end].iter().enumerate().rev() {
            if same_leaf_as_prev(paths, updates, idx) {
                continue;
            }

            self.node_unlock_write(paths, i.path, i.level().into());
        }
    }

    fn lock_updates_write(&self, paths: &mut Paths<'_>) -> Result<(), BchError> {
        ebug_assert!(!self.write_locked());

        let mut pu = paths.paths_and_updates();
        for idx in 0..pu.updates.len() {
            if same_leaf_as_prev(&pu.paths, pu.updates, idx) {
                continue;
            }

            let i = &pu.updates[idx];
            if let Err(e) = self.node_lock_write(&mut pu.paths, i.path, i.level().into()) {
                self.lock_write_fail(&mut pu, idx);
                return Err(e);
            }

            if !i.cached() {
                let b = unsafe { leaf(&pu.paths, i) };
                self.node_prep_for_write(&mut pu, b);
            }
        }

        self.set_write_locked(true);
        Ok(())
    }

    #[inline]
    fn unlock_updates_write(&self, paths: &mut Paths<'_>) {
        if likely(self.write_locked()) {
            let PathsUpdates { mut paths, updates } = paths.paths_and_updates();
            for i in updates.iter() {
                if paths[i.path].write_locked(i.level().into()) {
                    self.node_unlock_write_inlined(&mut paths, i.path, i.level().into());
                }
            }
            self.set_write_locked(false);
        }
    }

    fn insert_entry_checks(&self, paths: &Paths<'_>, i: &c::btree_insert_entry) {
        let fs = self.fs();
        let path = &paths[i.path];
        let k = i.key();

        assert!(k.k.p == path.pos);
        assert!(i.cached() == path.cached());
        assert!(!(i.cached() && !i.key_cache_already_flushed() && k.k.key_type().is_deleted()));
        assert!(i.level() == path.level());
        assert!(i.btree_id() == path.btree_id());
        assert!(u32::from(i.bkey_type) == BtreeNodeType::of(path.level().into(), path.btree_id()).0);
        ebug_assert!(!(i.level() == 0 &&
                       i.btree_id().has_snapshots() &&
                       i.flags & c::BTREE_UPDATE_internal_snapshot_node == 0 &&
                       fs.journal.flags.get(Journal::replay_done) &&
                       k.k.p.snapshot != 0 &&
                       fs.snapshots.is_leaf(k.k.p.snapshot) == Ok(false)));
    }

    #[cold]
    #[inline(never)]
    fn journal_transaction_names_changed(&self, res: &mut c::journal_res) -> BchError {
        self.fs().journal.res_put(res);
        self.restart(bch_errcode::BCH_ERR_transaction_restart_journal_overwrites_changed)
    }

    #[inline(always)]
    fn commit_journal_res_get(&self, paths: &mut Paths<'_>, res: &mut c::journal_res, flags: u32)
        -> Result<(), BchError>
    {
        let ret = self.fs().journal.res_get_waiting(res, self.journal_u64s.get(), flags, |list, cond, timeout| {
            self.wait_event_timeout_in(paths, list, cond, timeout)
        });
        self.journal_res_seq.set(res.seq);
        ret?;

        if unlikely(res.has_overwrites != self.journal_transaction_names()) {
            return Err(self.journal_transaction_names_changed(res));
        }
        Ok(())
    }

    /// This may unlock and relock.
    #[inline(never)]
    fn key_can_insert_cached_slowpath(&self, paths: &mut Paths<'_>, flags: CommitOpts, path: PathIdx, u64s: usize)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let (ck, btree) = (cached_entry(paths, path), paths[path].btree_id());

        if u64s > usize::from(unsafe { (*ck).u64s }) {
            let new_u64s = (u64s + u64s / 2).next_power_of_two();
            let bytes = new_u64s * size_of::<u64>();

            let first = {
                let _noreclaim = MemallocFlags::noreclaim();
                kmalloc(bytes)
            };
            let new_k = match first {
                Ok(new_k) => new_k,
                Err(_) => {
                    self.unlock_updates_write(paths);
                    self.unlock_from(paths, crate::ret_ip!());

                    let Ok(new_k) = kmalloc(bytes) else {
                        bch_err!(fs, "error allocating memory for key cache key, btree {btree} u64s {new_u64s}");
                        return fs.throw(bch_errcode::BCH_ERR_ENOMEM_btree_key_cache_insert);
                    };

                    if let Err(err) = self.relock_from(paths).and_then(|()| self.lock_updates_write(paths)) {
                        unsafe { c::rust_kfree(new_k.as_ptr().cast()) };
                        return Err(err);
                    }
                    new_k
                }
            };
            let new_k = new_k.as_ptr().cast::<c::bkey_i>();

            unsafe {
                let old_k = (*ck).k;
                ptr::copy_nonoverlapping(old_k.cast::<u64>(), new_k.cast::<u64>(), (*ck).u64s.into());
                c::rust_kfree(old_k.cast());

                for i in paths.paths_and_updates().updates.iter_mut() {
                    if ptr::eq(i.old_v, &raw const (*old_k).v) {
                        i.old_v = &raw const (*new_k).v;
                    }
                }

                (*ck).u64s = new_u64s as u16;
                (*ck).k    = new_k;
            }
        }

        if watermark(flags) < c::bch_watermark::reclaim as u32 &&
           !unsafe { &*ck }.dirty() &&
           fs.btree.key_cache.must_wait() &&
           fs.journal.flags.get(Journal::replay_done)
        {
            return fs.throw(bch_errcode::BCH_ERR_btree_insert_need_journal_reclaim);
        }

        Ok(())
    }

    #[inline]
    fn key_can_insert_cached(paths: &Paths<'_>, path: PathIdx, u64s: usize) -> bool {
        let ck = unsafe { &*cached_entry(paths, path) };

        likely(u64s <= usize::from(ck.u64s) && ck.dirty())
    }

    #[cold]
    #[inline(never)]
    fn accounting_revert(&self, t: &TransAttempt<'_, '_>, mark: &mut MarkRead<'_>, flags: CommitOpts, end: usize) {
        for (at, k) in self.accounting.get().iter(t).with_offsets() {
            if at >= end {
                break;
            }
            let a = k.key().arm::<Accounting>().expect("an accounting key");
            accounting_mem::trans_commit_revert(t, mark, a.k.p, &a.v.d, flags);
        }
    }

    #[cold]
    #[inline(never)]
    fn commit_fatal_err(&self, err: BchError) -> BchError {
        fs_fatal_error!(self.fs(), "{}(): fatal error in transaction commit: {err}", self.fn_name());
        err
    }

    #[cold]
    #[inline(never)]
    fn commit_invalid_entry(&self, err: BchError) -> BchError {
        let fs = self.fs();
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "{}(): invalid journal entry on insert\n", self.fn_name());
        self.updates_to_text(&mut msg);
        fs.sb_error_count(c::SbErrorId::validate_error_in_commit);
        fs.emergency_read_only(&mut msg);
        let _ = prt_task_backtrace(&mut msg, sched::current(), 1);
        err
    }

    #[cold]
    #[inline(never)]
    fn commit_invalid_key(&self, i: &c::btree_insert_entry, err: BchError) -> BchError {
        let fs = self.fs();
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "{}() -> ", self.fn_name());
        prt_symbol(&mut msg, i.ip_allocated);
        prt!(&mut msg, ": invalid bkey on insert from\n");
        self.updates_to_text(&mut msg);
        fs.sb_error_count(c::SbErrorId::validate_error_in_commit);
        fs.emergency_read_only(&mut msg);
        let _ = prt_task_backtrace(&mut msg, sched::current(), 1);
        err
    }

    /// Whether, checking, a key the transaction writes was overwritten in the
    /// journal since: the commit restarts.
    #[cold]
    fn check_drop_overwrites_from_journal(&self, t: &TransAttempt<'_, '_>, paths: &Paths<'_>, jk: &c::journal_keys,
                                          g: &mut journal_overlay::OverwriteGuard<'_>, check: bool) -> bool {
        /*
         * Accounting keys aren't deduped in the journal: we have to compare
         * each individual update against what's in the btree to see if it has
         * been applied yet, and accounting updates also don't overwrite,
         * they're deltas that accumulate.
         */
        let overwritten = paths.update_keys()
            .filter(|(_, k)| k.k.key_type() != BkeyType::accounting)
            .any(|(i, k)| journal_overlay::check_or_overwrite(jk, g, i.btree_id(), i.level().into(), k.k.p, check));
        if overwritten {
            return true;
        }

        for_each_journal_entry(t, |i| {
            if is_keys_entry(i) {
                for_each_entry_key(i, |k| {
                    if journal_overlay::check_or_overwrite(jk, g, Btree(i.btree_id.into()), i.level.into(), k.k.p,
                                                           check) {
                        Err(BchError::from(bch_errcode::BCH_ERR_transaction_restart_journal_overwrites_changed))
                    } else {
                        Ok(())
                    }
                })?;
            }
            Ok(())
        }).is_err()
    }

    #[cold]
    #[inline(never)]
    fn commit_to_journal_replay_pre<'k>(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>,
                                        jk: &'k c::journal_keys, flags: CommitOpts)
        -> Result<journal_overlay::OverwriteGuard<'k>, BchError>
    {
        let mut g = jk.overwrites.lock();

        if has(flags, CommitFlags::JOURNAL_REPLAY) &&
           self.check_drop_overwrites_from_journal(t, paths, jk, &mut g, true) {
            drop(g);
            self.unlock_updates_write(paths);
            return Err(self.restart(bch_errcode::BCH_ERR_transaction_restart_journal_overwrites_changed));
        }

        Ok(g)
    }

    #[cold]
    #[inline(never)]
    fn commit_to_journal_replay_post(&self, t: &TransAttempt<'_, '_>, paths: &Paths<'_>, jk: &c::journal_keys,
                                     mut g: journal_overlay::OverwriteGuard<'_>, ok: bool) {
        if ok {
            self.check_drop_overwrites_from_journal(t, paths, jk, &mut g, false);
        }
    }

    fn commit_has_extents(&self, paths: &Paths<'_>) -> bool {
        paths.updates().iter().any(|i| {
            matches!(i.btree_id(), Btree::extents | Btree::reflink | Btree::stripes) || i.level() != 0
        })
    }

    fn commit_error_inner(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, res: &mut c::journal_res,
                          flags: CommitOpts, i: Option<usize>, err: BchError)
        -> Result<(), BchError>
    {
        let fs = self.fs();

        if err.matches(bch_errcode::BCH_ERR_journal_res_blocked) {
            /*
             * XXX: this should probably be a separate BTREE_INSERT_NONBLOCK
             * flag
             */
            if has(flags, CommitFlags::JOURNAL_RECLAIM) && watermark(flags) < c::bch_watermark::reclaim as u32 {
                return fs.throw(bch_errcode::BCH_ERR_journal_reclaim_would_deadlock);
            }

            return t.drop_locks_do_in(paths, |paths| {
                self.commit_journal_res_get(paths, res, watermark(flags) | c::JOURNAL_RES_GET_CHECK)
            });
        }

        if err == bch_errcode::BCH_ERR_btree_insert_btree_node_full.into() {
            let i = paths.updates()[i.expect("a full node is an update's")];
            let flags = if self.commit_has_extents(paths) { flags.watermark_hipri() } else { flags };

            let ret = split_leaf(self, paths, i.path, i.key().k.u64s.into(), flags);
            if ret.is_ok() && self.has_interior_updates() {
                return Err(self.restart(bch_errcode::BCH_ERR_transaction_restart_split_with_interior_updates));
            }

            if ret.as_ref().is_err_and(is_restart) {
                fs.event_inc_trace(events::trans_restart_btree_node_split, |out| {
                    prt!(out, "{}\n", self.fn_name());
                    self.path_to_text(out, paths, i.path);
                });
            }
            return ret;
        }

        if err == bch_errcode::BCH_ERR_btree_insert_need_mark_replicas.into() {
            return t.drop_locks_do_in(paths, |_| crate::accounting::update_sb(t));
        }

        if err == bch_errcode::BCH_ERR_btree_insert_need_journal_reclaim.into() {
            self.unlock_from(paths, crate::ret_ip!());

            fs.event_inc_trace(events::trans_blocked_journal_reclaim, |out| {
                let kc = &fs.btree.key_cache;
                prt!(out, "{}\n", self.fn_name());
                prt!(out, "key cache dirty {}/{} must_wait {}\n",
                     kc.nr_keys.load(Relaxed), kc.nr_dirty.load(Relaxed), kc.dirty_over_max());
            });

            let times = &fs.times[c::bch_time_stats::blocked_key_cache_flush as usize];
            times.track_event_change(true);
            let ret = fs.journal.key_cache_reclaim_wait(|| self.unlock_long_from(paths, crate::ret_ip!()));
            times.track_event_change(false);

            ret?;
            return self.relock_from(paths);
        }

        Err(err)
    }

    #[inline(never)]
    fn commit_error(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, res: &mut c::journal_res,
                    flags: CommitOpts, i: Option<usize>, err: BchError)
        -> Result<(), BchError>
    {
        let fs = self.fs();
        let ret = self.commit_error_inner(t, paths, res, flags, i, err);

        assert!(ret.as_ref().is_err_and(is_restart) == (self.restarted().0 != 0));

        if let Some(err) = ret.as_ref().err().filter(|e| e.matches(c::ENOSPC) && has(flags, CommitFlags::NO_ENOSPC)) {
            fs_inconsistent!(fs, "bch2_trans_commit_error: incorrectly got {err}\n");
        }
        ret
    }

    /// The transaction's accounting keys applied to the in-memory counters,
    /// unless replay already did: @Err(at) at the one that failed, the ones
    /// before it applied.
    fn commit_accounting_apply(&self, t: &TransAttempt<'_, '_>, mark: &mut MarkRead<'_>, flags: CommitOpts)
        -> Result<(), (BchError, usize)>
    {
        if has(flags, CommitFlags::SKIP_ACCOUNTING_APPLY) {
            return Ok(());
        }

        for (at, k) in self.accounting.get().iter(t).with_offsets() {
            let a = k.key().arm::<Accounting>().expect("an accounting key");
            let usage = mem_add(self.fs(), Delta { p: a.k.p, d: &a.v.d }, false, mark).map_err(|err| (err, at))?;
            self.fs_usage_delta_add(usage);
        }
        Ok(())
    }

    fn journal_replay_insert_keys(&self, t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
        let fs = self.fs();

        for (i, k) in self.paths().update_keys() {
            journal_overlay::insert(fs, i.btree_id(), i.level().into(), k)?;
        }

        for_each_journal_entry(t, |i| {
            if is_keys_entry(i) {
                for_each_entry_key(i, |k| journal_overlay::insert(fs, Btree(i.btree_id.into()), i.level.into(), k))?;
            }

            if c::bch_jset_entry_type(i.type_.into()) == c::bch_jset_entry_type::btree_root {
                let e = <c::jset_entry<Unvalidated> as Unsized>::from_raw_mut(ptr::from_ref(i).cast_mut().cast(),
                                                                             size_of_val(i));
                fs.journal_entry_to_btree_root(unsafe { &mut *e });
            }
            Ok(())
        })?;

        self.accounting.get().iter(t).try_for_each(|k| journal_overlay::insert(fs, Btree::accounting, 0, k.key()))
    }

    /*
     * This is for updates done in the early part of fsck - btree_gc - before we've
     * gone RW. we only add the new key to the list of keys for journal replay to
     * do.
     */
    #[inline(never)]
    fn commit_to_journal_replay(&self, t: &TransAttempt<'_, '_>, flags: CommitOpts) -> Result<(), BchError> {
        let fs = self.fs();

        /* we're actually shutting down */
        if fs.flags.get(c::bch_fs_flags::may_go_rw) {
            return fs.throw(bch_errcode::BCH_ERR_erofs_trans_commit);
        }

        assert!(sched::current() == fs.recovery_task.load(Relaxed));

        loop {
            self.fs_usage_delta.set(Default::default());

            let ret = {
                let mut mark = fs.mark_lock().read();

                match self.commit_accounting_apply(t, &mut mark, flags) {
                    Ok(()) => {
                        /* Only fatal errors are possible later, so no need to revert this */
                        account_disk_usage_change(t);
                        drop(mark);

                        match self.journal_replay_insert_keys(t) {
                            Ok(()) => {
                                fs.event_inc_trace(events::transaction_commit, |out| {
                                    prt!(out, "{}\nseq {}\n", self.fn_name(), self.journal_res_seq.get());
                                    self.updates_to_text(out);
                                });
                                return Ok(());
                            }
                            Err(err) => {
                                fs_fatal_error!(fs, "fatal error in transaction commit: {err}");
                                let mut mark = fs.mark_lock().read();
                                self.accounting_revert(t, &mut mark, flags, usize::MAX);
                                err
                            }
                        }
                    }
                    Err((err, at)) => {
                        self.accounting_revert(t, &mut mark, flags, at);
                        err
                    }
                }
            };

            if !ret.matches(bch_errcode::BCH_ERR_btree_insert_need_mark_replicas) {
                return Err(ret);
            }
            t.drop_locks_do(|| crate::accounting::update_sb(t))?;
        }
    }

    #[cold]
    #[inline(never)]
    fn commit_extra_disk_res(&self, t: &TransAttempt<'_, '_>, flags: CommitOpts) -> Result<(), BchError> {
        /* Every site that charges extra_disk_res records what it charged at */
        ebug_assert!(self.extra_disk_res_replicas.get() != 0);

        let res = t.disk_res().expect("extra_disk_res charged with no disk reservation");
        res.add_physical(self.extra_disk_res.get(), self.extra_disk_res_replicas.get().into(),
                         if has(flags, CommitFlags::NO_ENOSPC) {
                             c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL
                         } else {
                             c::bch_reservation_flags::empty()
                         })
    }

    /*
     * extents and reflink reference data: bch2_bkey_set_needs_reconcile() may pad a
     * key with invalid-device pointers to restore its accounted durability, and
     * charges extra_disk_res for them - so a leaf update needs somewhere to put it.
     *
     * BTREE_TRIGGER_norun writes the key verbatim, so nothing can charge and the
     * reservation can't be needed - that's how journal replay and the fsck
     * error-injection tests lay down deliberately bad keys.
     */
    fn verify_disk_res(&self, paths: &Paths<'_>) {
        if !cfg!(CONFIG_BCACHEFS_DEBUG) || !self.disk_res.get().is_null() {
            return;
        }

        for i in paths.updates() {
            crate::warn_once!(i.level() == 0 &&
                              i.flags & c::BTREE_TRIGGER_norun == 0 &&
                              matches!(i.btree_id(), Btree::extents | Btree::reflink),
                              self.fs(), "{}(): update to {} without a disk reservation",
                              self.fn_name(), i.btree_id());
        }
    }

    /*
     * Finish compacting the updates array, copying down the entries we
     * hadn't scanned yet: called when trans->updates must be made
     * consistent mid-compaction - on error, so that
     * bch2_trans_reset_updates() sees each surviving path exactly once
     * (the noops' paths have already been put, and entries between dst
     * and i are stale duplicates), and before btree node merge.
     */
    #[cold]
    #[inline(never)]
    fn commit_compact_tail(&self, updates: &mut [c::btree_insert_entry], dst: usize, i: usize) {
        updates.copy_within(i + 1.., dst);
        self.nr_updates.set((dst + updates.len() - (i + 1)) as _);
    }

    #[cold]
    #[inline(never)]
    fn commit_merge(&self, paths: &mut Paths<'_>, flags: CommitOpts, i: &mut usize, dst: &mut usize)
        -> Result<(), BchError>
    {
        let Some(_write) = self.fs().writes.tryget_live() else {
            return Ok(());
        };

        /* Put trans->updates in a consistent state in case we error: */
        let PathsUpdates { updates, .. } = paths.paths_and_updates();
        self.commit_compact_tail(updates, *dst, *i);
        *i = *dst - 1;

        let (path, level) = (updates[*i].path, updates[*i].level().into());
        merge(self, paths, path, level, flags.watermark_hipri(), None)
    }

    fn update_is_noop(i: &c::btree_insert_entry, flags: CommitOpts) -> bool {
        let old: BkeySC<'_> = BkeySC { k: &i.old_k, v: i.old_v, iter: PhantomData, val: PhantomData };

        /*
         * We can't drop noop inode updates because fsync relies
         * on grabbing the journal_seq of the latest update from
         * the inode - and the journal_seq isn't updated until
         * the atomic trigger:
         */
        likely(!has(flags, CommitFlags::NO_SKIP_NOOPS) &&
               u32::from(i.bkey_type) != BtreeNodeType::of(0, Btree::inodes).0 &&
               old.and_val_eq(BkeySC::from(i.key())))
    }

    /*
     * Write buffer btree updates are never authored on their own: they're emitted
     * by a trigger on an update to some other btree. So if update_is_noop() dropped
     * an update and all that's left is write buffer keys, those keys are the
     * dropped update's trigger output - describing a change that isn't happening,
     * and noops themselves.
     *
     * They can't be recognized one at a time the way update_is_noop() does: a write
     * buffer update carries no old key to compare against. @dropped_noops is what
     * makes it decidable - without it this is indistinguishable from a transaction
     * that only ever meant to write buffered keys.
     *
     * Committing them anyway is not just wasted work. bch2_trans_commit_lazy()
     * signals success as transaction_restart_commit, so a caller looping over a
     * write buffer btree re-drives at the same position, cannot see its own
     * buffered write, and reissues the identical commit forever.
     */
    fn commit_became_noop(&self, t: &TransAttempt<'_, '_>, dropped_noops: bool) -> bool {
        if !dropped_noops || self.nr_updates.get() != 0 || self.accounting.get().u64s != 0 {
            return false;
        }

        for_each_journal_entry(t, |i| {
            if c::bch_jset_entry_type(i.type_.into()) != c::bch_jset_entry_type::write_buffer_keys {
                return Err(BchError::from(c::EINVAL));
            }

            /*
             * fsck repairs accounting directly, not as a consequence of
             * another key changing: not derived, and not ours to drop.
             */
            for_each_entry_key(i, |k| if k.k.key_type() == BkeyType::accounting {
                Err(BchError::from(c::EINVAL))
            } else {
                Ok(())
            })
        }).is_ok()
    }

    #[inline]
    fn commit_journal_fill(&self, t: &TransAttempt<'_, '_>, paths: &Paths<'_>, res: &mut c::journal_res) {
        let names = self.journal_transaction_names();

        /*
         * The reservation is commit_updates()' journal_u64s, these entries
         * summed: each has the room it's sized for.
         */
        self.fs().journal.res_append_entries(res, |w| {
            if names {
                invariant!(w.room() >= jset_u64s(JSET_ENTRY_LOG_U64S));
                let log = w.add_entry(c::bch_jset_entry_type::log, 0, 0, JSET_ENTRY_LOG_U64S)?.as_mut_bytes();
                let name = unsafe { CStr::from_ptr(self.fn_.get().cast()) }.to_bytes();
                let (n, pad) = log.split_at_mut(name.len().min(log.len()));
                n.copy_from_slice(&name[..n.len()]);
                pad.fill(0);
            }

            for (i, k) in paths.update_keys() {
                if i.key_cache_already_flushed() || i.flags & c::BTREE_UPDATE_nojournal != 0 {
                    continue;
                }

                unsafe { bch2_verify_update_old_key(self, i) };

                let (btree, level) = (i.btree_id().0 as u8, i.level());
                if names {
                    let val = if i.old_k.val_u64s() == 0 {
                        &[][..]
                    } else {
                        unsafe { core::slice::from_raw_parts(i.old_v.cast::<u64>(), i.old_k.val_u64s()) }
                    };
                    invariant!(w.room() >= jset_u64s(i.old_k.u64s.into()));
                    let e = w.add_entry(c::bch_jset_entry_type::overwrite, btree, level, i.old_k.u64s.into())?;
                    let (ek, ev) = e.split_at_mut(BKEY_U64S);
                    ek.as_mut_bytes().copy_from_slice(plain_bytes(&i.old_k));
                    ev.copy_from_slice(val);
                }

                invariant!(w.room() >= jset_u64s(k.k.u64s.into()));
                w.add_entry(c::bch_jset_entry_type::btree_keys, btree, level, k.k.u64s.into())?
                    .as_mut_bytes().copy_from_slice(k.as_bytes());
            }

            t.subbuf_with(&self.journal_entries, |e| {
                invariant!(w.room() >= e.len());
                w.push_entries(e)
            })?;

            t.subbuf_with(&self.accounting, |a| -> Result<(), NoRoom> {
                if !a.is_empty() {
                    invariant!(w.room() >= jset_u64s(a.len()));
                    w.add_entry(c::bch_jset_entry_type::write_buffer_keys, Btree::accounting.0 as u8, 0, a.len())?
                        .copy_from_slice(a);
                }
                Ok(())
            })
        }).expect("the commit's journal reservation, sized for its entries");
    }

    #[inline]
    fn commit_write_locked(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, res: &mut c::journal_res,
                           flags: CommitOpts, stopped_at: &mut Option<usize>)
        -> Result<(), BchError>
    {
        let fs = self.fs();

        self.verify_not_unlocked_or_in_restart();

        /*
         * Check if the insert will fit in the leaf node with the write lock
         * held, otherwise another thread could write the node changing the
         * amount of space available:
         */

        prefetch(&fs.journal.flags);

        let mut u64s = 0;
        let mut pu = paths.paths_and_updates();
        for idx in 0..pu.updates.len() {
            /* Multiple inserts might go to same leaf: */
            if !same_leaf_as_prev(&pu.paths, pu.updates, idx) {
                u64s = 0;
            }

            let i = &pu.updates[idx];
            u64s += usize::from(i.key().k.u64s);

            let ret = if !i.cached() {
                let b = unsafe { leaf(&pu.paths, i) };
                if b.insert_fits(unsafe { b.contents() }, u64s) {
                    Ok(())
                } else {
                    fs.throw(bch_errcode::BCH_ERR_btree_insert_btree_node_full)
                }
            } else {
                /* Slack for varint::decode_fast(), which reads up to 7 bytes past the end: */
                let u64s = u64s + 1;

                if Self::key_can_insert_cached(&pu.paths, i.path, u64s) {
                    Ok(())
                } else {
                    let path = i.path;
                    drop(pu);
                    let ret = self.key_can_insert_cached_slowpath(paths, flags, path, u64s);
                    pu = paths.paths_and_updates();
                    ret
                }
            };
            if let Err(err) = ret {
                *stopped_at = Some(idx);
                return Err(err);
            }

            unsafe { (*pu.updates[idx].key_ptr()).k.format_bits.set_needs_whiteout(0) };
        }
        drop(pu);

        let no_journal_res = has(flags, CommitFlags::NO_JOURNAL_RES);

        /*
         * Don't get journal reservation until after we know insert will
         * succeed:
         */
        if likely(!no_journal_res) {
            self.commit_journal_res_get(paths, res, watermark(flags) | c::JOURNAL_RES_GET_NONBLOCK)?;
        }

        /*
         * Not allowed to fail after we've gotten our journal reservation - we
         * have to use it:
         */

        if cfg!(CONFIG_BCACHEFS_DEBUG) && !no_journal_res {
            let seq = res.seq;
            for i in paths.updates() {
                let k = unsafe { &mut (*i.key_ptr()).k };
                if crate::debug::params::journal_seq_verify() {
                    k.bversion.lo = seq;
                } else if crate::debug::params::inject_invalid_keys() {
                    k.bversion = Bversion::MAX;
                }
            }
        }

        let mut h = self.hooks.get();
        while !h.is_null() {
            let hook = unsafe { &*h };
            let f = hook.fn_.expect("a commit hook with no function");
            ret_to_result(unsafe { f(ptr::from_ref(self).cast_mut(), h) })?;
            h = hook.next;
        }

        {
            let mut mark = fs.mark_lock().read();

            let hooked = self.accounting.get().iter_mut(t).with_offsets().try_for_each(|(at, mut k)| {
                let a = k.key_mut().arm::<Accounting>().expect("an accounting key");
                accounting_mem::trans_commit_hook(t, &mut mark, res, &mut a.k, &a.v.d, at / size_of::<u64>(), flags)
                    .map_err(|err| (err, at))
            });
            if let Err((err, at)) = hooked {
                self.accounting_revert(t, &mut mark, flags, at);
                return Err(err);
            }

            let d = self.fs_usage_delta.get();
            if d.hidden | d.btree | d.data | d.cached | d.reserved != 0 {
                account_disk_usage_change(t);
            }
        }

        unsafe { run_mem_triggers(self, t) }.map_err(|err| self.commit_fatal_err(err))?;

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            let mut validate_context = c::bkey_validate_context::default();
            validate_context.set_from(c::bkey_validate_from::commit);

            if !no_journal_res {
                validate_context.set_flags(ValidateFlags::write | ValidateFlags::commit);
            }

            t.subbuf_with(&self.journal_entries, |buf| {
                let mut buf = buf.as_mut_bytes();
                while !buf.is_empty() {
                    let u64s = u16::from_le_bytes([buf[0], buf[1]]);
                    let (e, rest) = buf.split_at_mut(jset_u64s(u64s.into()) * size_of::<u64>());
                    journal_entry_validate(fs, e, c::bcachefs_metadata_version_current, cfg!(target_endian = "big"),
                                           validate_context)
                        .map_err(|err| self.commit_invalid_entry(err))?;
                    buf = rest;
                }
                Ok::<_, BchError>(())
            })?;

            for i in paths.updates() {
                validate_context.level = i.level();
                validate_context.btree = i.btree_id();

                bkey_validate(&BkeyValidate { fs, k: BkeySC::from(i.key()), from: &validate_context })
                    .map_err(|err| self.commit_invalid_key(i, err))?;
                self.insert_entry_checks(paths, i);
            }
        }

        fs.event_inc_trace(events::transaction_commit, |out| {
            prt!(out, "{}\nseq {}\n", self.fn_name(), self.journal_res_seq.get());
            self.updates_to_text(out);
        });

        if likely(!no_journal_res) {
            let j = &fs.journal;

            self.commit_journal_fill(t, paths, res);

            if let Some(seq) = unsafe { self.journal_seq.get().as_mut() } {
                *seq = res.seq;
            }

            if unlikely(!self.journal_pin.get().is_null()) {
                j.pin_set(res.seq, unsafe { &*self.journal_pin.get() }, c::journal_pin_type::JOURNAL_PIN_TYPE_other,
                          Some(commit_journal_pin_flush));
            }

            if !self.flush.get().is_null() {
                unsafe { crate::journal::journal::__bch2_journal_flush_seq_async(j, res.seq, self.flush.get()) };
            }
        } else {
            /* No reservation, so the caller named the seq to pin: */
            self.journal_res_seq.set(self.journal_seq_to_pin.get());
        }

        let seq = self.journal_res_seq.get();
        let mut pu = paths.paths_and_updates();
        for idx in 0..pu.updates.len() {
            let i = &pu.updates[idx];

            if !i.cached() {
                let (path, k) = (i.path, i.key_ptr());
                unsafe { self.insert_key_leaf(&mut pu, path, &mut *<BkeyI>::from_raw_mut(k), seq) };
            } else if !i.key_cache_already_flushed() {
                unsafe { bch2_btree_insert_key_cached(self, &mut pu.paths, &raw mut pu.updates[idx]) };
            } else {
                let path = &raw mut pu.paths[i.path];
                unsafe { bch2_btree_key_cache_drop(self, &mut pu.paths, path) };
            }
        }

        Ok(())
    }

    /*
     * Get journal reservation, take write locks, and attempt to do btree update(s):
     */
    #[inline]
    fn do_commit(&self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, res: &mut c::journal_res,
                 flags: CommitOpts, stopped_at: &mut Option<usize>)
        -> Result<(), BchError>
    {
        /*
         * Pre-grow journal entries subbuf for write buffer updates
         * that will be added during atomic triggers (can't realloc
         * after journal reservation):
         */
        let extra = self.extra_journal_u64s.get();
        if extra != 0 {
            t.subbuf_reserve(&self.journal_entries, extra as usize)?;
        }

        self.lock_updates_write(paths)?;

        let jk = unlikely(self.journal_replay_not_finished()).then(|| unsafe { self.fs().journal_keys_held() });
        let overwrites = match jk {
            Some(jk) => Some(self.commit_to_journal_replay_pre(t, paths, jk, flags)?),
            None     => None,
        };

        let ret = self.commit_write_locked(t, paths, res, flags, stopped_at);

        if let (Some(jk), Some(g)) = (jk, overwrites) {
            self.commit_to_journal_replay_post(t, paths, jk, g, ret.is_ok());
        }

        self.unlock_updates_write(paths);

        if unlikely(res.ref_) {
            self.fs().journal.res_put(res);
        }

        ret
    }

    #[inline(never)]
    fn commit_btree_write_ratelimit(&self, t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
        let bc = &self.fs().btree.cache;

        t.drop_locks_do(|| {
            self.wait_event(&bc.nr_in_flight_wait, || {
                bc.nr_in_flight_inner.load(Relaxed) < (BTREE_WRITE_IO_LIMIT * 3 / 4) as isize &&
                    !bc.should_throttle.load(Relaxed)
            });
            Ok(())
        })
    }

    fn commit_updates(&self, t: &TransAttempt<'_, '_>, mut flags: CommitOpts, lazy: bool) -> Result<(), BchError> {
        let fs = self.fs();

        self.verify_not_unlocked_or_in_restart();

        self.maybe_inject_restart(crate::ret_ip!())?;

        if !self.has_updates() {
            return Ok(());
        }

        if watermark(flags) <= c::bch_watermark::normal as u32 &&
           likely(fs.journal.flags.get(Journal::replay_done)) &&
           unlikely(fs.btree.cache.should_throttle.load(Relaxed))
        {
            self.commit_btree_write_ratelimit(t)?;
        }

        /*
         * Extents updates release open buckets, which is what the journal
         * throttles on - they have to be elevated before we take a journal
         * reservation, not just in the btree node alloc path.
         */
        if self.commit_has_extents(&self.paths()) {
            flags = flags.watermark_hipri();
        }

        unsafe { run_trans_triggers(self, t) }?;

        let mut journal_u64s = 0;

        let names = fs.opts().journal_transaction_names.get() != 0;
        self.set_journal_transaction_names(names);
        if names {
            journal_u64s += jset_u64s(JSET_ENTRY_LOG_U64S);
        }

        let accounting_u64s = usize::from(self.accounting.get().u64s);
        if accounting_u64s != 0 {
            journal_u64s += jset_u64s(accounting_u64s);
        }

        let mut u64s_delta: i32 = 0;
        let mut dropped_noops = false;
        let (mut idx, mut dst) = (0, 0);
        let mut paths = self.paths();
        let mut pu = paths.paths_and_updates();
        while idx < pu.updates.len() {
            if unlikely(Self::update_is_noop(&pu.updates[idx], flags)) {
                self.path_put(&mut pu.paths, pu.updates[idx].path, true);
                dropped_noops = true;
                idx += 1;
                continue;
            }

            pu.updates[dst] = pu.updates[idx];
            dst += 1;

            let i = &pu.updates[idx];
            ebug_assert!(pu.paths[i.path].should_be_locked());

            if let Err(err) = self.path_upgrade(&mut pu.paths, i.path, usize::from(i.level()) + 1, crate::ret_ip!()) {
                self.commit_compact_tail(pu.updates, dst, idx);
                return Err(err);
            }

            ebug_assert!(pu.paths[i.path].intent_locked(i.level().into()));

            if i.key_cache_already_flushed() || i.flags & c::BTREE_UPDATE_nojournal != 0 {
                idx += 1;
                continue;
            }

            let k = i.key();

            /* we're going to journal the key being updated: */
            journal_u64s += jset_u64s(k.k.u64s.into());

            /* and we're also going to log the overwrite: */
            if names {
                journal_u64s += jset_u64s(i.old_k.u64s.into());
            }

            if !i.cached() {
                u64s_delta += if !k.k.key_type().is_deleted() { i32::from(k.k.u64s) } else { 0 };
                u64s_delta -= i32::from(i.old_btree_u64s);

                if !same_leaf_as_next(&pu.paths, pu.updates, idx) && !self.has_interior_updates() {
                    let b = unsafe { leaf(&pu.paths, i) };

                    if unlikely(b.needs_merge(unsafe { b.contents() }, fs, u64s_delta)) {
                        drop(pu);
                        self.commit_merge(&mut paths, flags, &mut idx, &mut dst)?;
                        pu = paths.paths_and_updates();
                    }

                    u64s_delta = 0;
                }
            }
            idx += 1;
        }
        drop(pu);

        self.nr_updates.set(dst as _);
        if !self.has_updates() || self.commit_became_noop(t, dropped_noops) {
            return Ok(());
        }

        self.verify_disk_res(&paths);

        if unlikely(self.extra_disk_res.get() != 0) {
            self.commit_extra_disk_res(t, flags)?;
        }

        let _write = if !has(flags, CommitFlags::NO_CHECK_RW) {
            match fs.writes.tryget_live() {
                Some(w) => Some(w),
                None    => {
                    drop(paths);
                    return self.commit_to_journal_replay(t, flags);
                }
            }
        } else {
            None
        };

        ebug_assert!(!fs.flags.get(c::bch_fs_flags::clean_shutdown));

        let mut res = c::journal_res::default();
        loop {
            let mut errored_at = None;

            self.verify_not_unlocked_or_in_restart();
            if likely(!has(flags, CommitFlags::NO_JOURNAL_RES)) {
                res = Default::default();
                self.journal_res_seq.set(0);
            }
            self.fs_usage_delta.set(Default::default());

            self.journal_u64s.set((journal_u64s + usize::from(self.journal_entries.get().u64s) +
                                   self.extra_journal_u64s.get() as usize) as u32);

            let ret = self.do_commit(t, &mut paths, &mut res, flags, &mut errored_at);

            /* make sure we didn't drop or screw up locks: */
            self.verify_locks(&paths);

            match ret {
                Ok(())   => break,
                Err(err) => self.commit_error(t, &mut paths, &mut res, flags, errored_at, err)?,
            }
        }

        self.commit_count.set(self.commit_count.get() + 1);
        drop(_write);

        self.downgrade(&mut paths);

        /*
         * Lazy commits signal success via transaction_restart_commit; a real
         * error must not be masked into a restart, or the caller's restart
         * loop retries a failing commit forever:
         */
        if lazy {
            return Err(self.restart(bch_errcode::BCH_ERR_transaction_restart_commit));
        }
        Ok(())
    }

    pub(crate) fn commit_trans(&self, flags: CommitOpts, lazy: bool) -> Result<(), BchError> {
        let t = unsafe { self.attempt_in_progress() };
        let ret = self.commit_updates(&t, flags, lazy);
        self.reset_updates(&mut self.paths());
        ret
    }
}

unsafe extern "C" fn commit_journal_pin_flush(_j: *mut c::journal, _pin: *mut c::journal_entry_pin, _seq: u64) -> c_int {
    0
}

#[c_export]
pub fn __bch2_trans_commit(trans: &BtreeTrans<'_>, flags: c::bch_trans_commit_flags, lazy: bool) -> c_int {
    ret_to_c(trans.commit_trans(flags.into(), lazy))
}
