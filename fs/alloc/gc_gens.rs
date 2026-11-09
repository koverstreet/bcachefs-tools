// SPDX-License-Identifier: GPL-2.0

//! gc_gens: each bucket's oldest_gen brought up to its gen, so gens can wrap.

use crate::alloc::background::alloc_to_v4;
use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::POS_MIN;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, CommitFlags, UpdateTriggerFlags};
use crate::c;
use crate::data::ptrs::drop_stale_ptrs;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, BchError};
use crate::format::Btree;
use crate::fs::Fs;
use crate::init::passes::{self, RecoveryPass};
use crate::sb::members::DevRef;
use crate::types::Le64;
use crate::util::alloc::KVVec;
use crate::util::kernel::local_clock;
use crate::util::rcu::read_lock;
use crate::util::time_stats::TimeStats;
use crate::util::Printbuf;
use crate::{bch_err, btree_trans};

fn recalc_oldest_gens(fs: &Fs, start_time: u64) -> Result<(), BchError> {
    let enomem = |_| fs.err(bch_errcode::BCH_ERR_ENOMEM_gc_gens);

    let mut oldest_gens: KVVec<(DevRef, KVVec<u8>)> = KVVec::new();
    for ca in fs.members() {
        let mi = ca.mi.get();
        let mut oldest = KVVec::from_elem(0u8, mi.nbuckets as usize).map_err(enomem)?;

        let rcu = read_lock();
        for b in u64::from(mi.first_bucket)..mi.nbuckets {
            oldest[b as usize] = ca.bucket_gen(b, &rcu).expect("one of the device's buckets");
        }
        drop(rcu);

        oldest_gens.push((ca, oldest)).map_err(enomem)?;
    }

    let trans = btree_trans!(fs);

    /*
     * Dropping a stale pointer is not an allocation, but the reconcile
     * trigger may pad the key with invalid-device pointers to restore its
     * accounted durability, and charges extra_disk_res for those - so the
     * commit needs a reservation to put the charge in. Released per key so
     * it can't accumulate across the walk.
     */
    let res = DiskReservation::new(fs);

    for btree in (0..Btree::NR).map(Btree).filter(|b| b.has_data_ptrs()) {
        fs.gc_gens.pos.set(c::bbpos { btree, pos: POS_MIN });

        BtreeIter::new(&trans, btree, POS_MIN, BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS)
            .for_each_commit(&trans, Some(&res), CommitFlags::NO_ENOSPC, |t, iter, k| {
                res.put();

                if fs.flags.get(c::bch_fs_flags::going_ro) {
                    return Err(BchError::from(c::EROFS));
                }

                drop_stale_ptrs(t, iter, k)
            })?;
    }

    BtreeIter::new(&trans, Btree::alloc, POS_MIN, BtreeIterFlags::PREFETCH)
        .for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
            let Some((_, oldest)) = oldest_gens.iter().find(|(ca, _)| u64::from(ca.dev_idx) == k.k.p.inode) else {
                return Ok(());
            };
            let Some(&oldest_gen) = oldest.get(k.k.p.offset as usize) else { return Ok(()) };

            if alloc_to_v4(k).oldest_gen == oldest_gen {
                return Ok(());
            }

            let mut a = t.alloc_to_v4_mut(k)?;
            a.v_mut().oldest_gen = oldest_gen;
            t.update(iter, &a, UpdateTriggerFlags::empty())
        })?;

    fs.gc_gens.pos.set(c::bbpos::default());

    fs.times[c::bch_time_stats::btree_gc as usize].update(start_time);
    fs.event_inc_trace(events::gc_gens_end, |_| {});

    let no_stale_ptrs = 1 << c::bch_sb_compat::no_stale_ptrs.0;
    if fs.sb.compat.get() & no_stale_ptrs == 0 {
        let mut sb = fs.sb_lock();
        let compat = &mut sb.sb_mut().compat[0];
        *compat = Le64::new(compat.get() | no_stale_ptrs);
        fs.write_super(&mut sb);
    }

    Ok(())
}

fn gc_gens(fs: &Fs) -> Result<(), BchError> {
    let start_time = local_clock();

    fs.event_inc_trace(events::gc_gens_start, |_| {});

    /*
     * We have to use trylock here. Otherwise, we would
     * introduce a deadlock in the RO path - we take the
     * state lock at the start of going RO.
     */
    let Some(_state) = fs.state_lock.try_read() else { return Ok(()) };

    recalc_oldest_gens(fs, start_time)
}

crate::recovery_pass!(gc_gens => |fs: &Fs| match gc_gens(fs) {
    /* going read-only isn't the pass failing */
    Err(e) if e.matches(c::EROFS) => Ok(()),
    ret => ret,
});

impl Fs {
    /// gc_gens, run now - serialized with the recovery passes it's one of.
    pub fn gc_gens(&self) -> Result<(), BchError> {
        let _run = passes::run_lock(self);

        let ret = gc_gens(self);
        if let Err(e) = &ret {
            if !e.matches(c::EROFS) {
                bch_err!(self, "gc_gens(): error {}", e.msg());
            }
        }
        ret
    }

    /// gc_gens, scheduled to run in the background.
    pub fn gc_gens_async(&self) {
        let _ = passes::run_explicit(self, &mut Printbuf::new(), RecoveryPass::gc_gens,
                                     c::bch_run_recovery_pass_flags::RUN_RECOVERY_PASS_ephemeral);
    }
}
