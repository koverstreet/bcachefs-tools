// SPDX-License-Identifier: GPL-2.0

use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::{pos, spos, BkeyCookie, BkeyS, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::iter::{
    commit_do, lockrestart_do, trans_commit_do, BtreeIter, BtreeIterFlags, BtreeNodeIter,
    BtreeTrans, CommitFlags, CommitOpts, TransAttempt, TransError, UpdateTriggerFlags,
};
use crate::data::extents::{
    bkey_extent_entries_mut, bkey_extent_entries_sc, bkey_ptrs_mut, entry_stripe_ptr_mut,
    extent_entry_type,
};
use crate::errcode::{
    bch_err_throw,
    bch_errcode,
    BchError,
    ENOENT_bkey_type_mismatch,
};
use crate::fs::{BorrowedFs, Fs};
use crate::util::async_exec::{block_on, spawn, system_unbound, WaitGroup};
use crate::util::kernel::{local_clock, random_u64_below};
use crate::util::printbuf::Printbuf;
use crate::c;

use core::ffi::{c_char, c_int, CStr};
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

#[cfg(kernel)]
use kernel::prelude::*;
#[cfg(kernel)]
use kernel::sync::Arc;
#[cfg(not(kernel))]
use std::sync::Arc;

#[cfg(not(kernel))]
use bcachefs_shim as kernel;

type TestRet = Result<(), BchError>;
type TestFn = fn(&Fs, u64) -> TestRet;

const ENOMEM: i32 = 12;
const NO_ENOSPC: CommitOpts = CommitOpts::new().flags(CommitFlags::NO_ENOSPC);
const INTERNAL_SNAPSHOT_NODE: UpdateTriggerFlags = UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE;

fn error_ret(error: BchError) -> i32 {
    -error.raw()
}

fn errcode(code: bch_errcode) -> i32 {
    error_ret(bch_err_throw(code))
}

fn enomem() -> BchError {
    BchError::from_raw(ENOMEM)
}

fn round_up(v: u64, by: u64) -> u64 {
    ((v + by - 1) / by) * by
}

fn delete_test_keys(fs: &Fs) -> TestRet {
    fs.btree_delete_range(
        c::btree_id::extents,
        spos(0, 0, u32::MAX),
        pos(0, u64::MAX),
        BtreeIterFlags::empty(),
    )?;

    fs.btree_delete_range(
        c::btree_id::xattrs,
        spos(0, 0, u32::MAX),
        pos(0, u64::MAX),
        BtreeIterFlags::empty(),
    )
}

fn insert_cookie(fs: &Fs, btree: c::btree_id, k: &mut BkeyCookie) -> TestRet {
    fs.btree_insert(
        btree,
        k,
        None,
        CommitOpts::new(),
        BtreeIterFlags::empty(),
    )
}

fn trans_cookie_alloc<'a, 't>(
    t: &TransAttempt<'a, 't>,
) -> Result<crate::btree::iter::TransBkey<'a, 't>, BchError> {
    t.bkey_alloc_typed::<c::bkey_i_cookie>()
}

fn test_delete(fs: &Fs, _nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(
        &trans,
        c::btree_id::xattrs,
        spos(0, 0, u32::MAX),
        BtreeIterFlags::INTENT,
    );

    commit_do(&trans, None, NO_ENOSPC, |t| {
        let mut k = trans_cookie_alloc(&t)?;
        k.k_mut().set_snapshot(u32::MAX);
        let t = iter.traverse(t)?;
        t.update(&mut iter, k, UpdateTriggerFlags::empty())
    })?;

    commit_do(&trans, None, NO_ENOSPC, |t| {
        let t = iter.traverse(t)?;
        t.delete_at(&mut iter, UpdateTriggerFlags::empty())
    })?;

    commit_do(&trans, None, NO_ENOSPC, |t| {
        let t = iter.traverse(t)?;
        t.delete_at(&mut iter, UpdateTriggerFlags::empty())
    })
}

fn test_delete_written(fs: &Fs, _nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(
        &trans,
        c::btree_id::xattrs,
        spos(0, 0, u32::MAX),
        BtreeIterFlags::INTENT,
    );

    commit_do(&trans, None, NO_ENOSPC, |t| {
        let mut k = trans_cookie_alloc(&t)?;
        k.k_mut().set_snapshot(u32::MAX);
        let t = iter.traverse(t)?;
        t.update(&mut iter, k, UpdateTriggerFlags::empty())
    })?;

    trans.unlock();
    fs.journal_flush_outstanding_pins();

    commit_do(&trans, None, NO_ENOSPC, |t| {
        let t = iter.traverse(t)?;
        t.delete_at(&mut iter, UpdateTriggerFlags::empty())
    })
}

fn test_iterate(fs: &Fs, nr: u64) -> TestRet {
    delete_test_keys(fs)?;

    for i in 0..nr {
        let mut k = BkeyCookie::new();
        k.k_mut().p.offset = i;
        k.k_mut().p.snapshot = u32::MAX;
        insert_cookie(fs, c::btree_id::xattrs, &mut k)?;
    }

    let trans = BtreeTrans::new(fs);
    let mut i = 0;
    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, 0, u32::MAX), BtreeIterFlags::empty());
    iter.for_each_max(&trans, pos(0, u64::MAX), |k| {
        assert!(k.k.p.offset == i);
        i += 1;
        ControlFlow::Continue(())
    })?;
    assert_eq!(i, nr);

    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, u64::MAX, u32::MAX), BtreeIterFlags::empty());
    iter.for_each_reverse(&trans, POS_MIN, |k| {
        i -= 1;
        assert!(k.k.p.offset == i);
        ControlFlow::Continue(())
    })?;
    assert_eq!(i, 0);
    Ok(())
}

fn test_iterate_extents(fs: &Fs, nr: u64) -> TestRet {
    delete_test_keys(fs)?;

    for i in (0..nr).step_by(8) {
        let mut k = BkeyCookie::new();
        k.k_mut().p.offset = i + 8;
        k.k_mut().p.snapshot = u32::MAX;
        k.k_mut().size = 8;
        insert_cookie(fs, c::btree_id::extents, &mut k)?;
    }

    let trans = BtreeTrans::new(fs);
    let mut i = 0;
    let mut iter = BtreeIter::new(&trans, c::btree_id::extents, spos(0, 0, u32::MAX), BtreeIterFlags::empty());
    iter.for_each_max(&trans, pos(0, u64::MAX), |k| {
        assert_eq!(k.k.start_offset(), i);
        i = k.k.p.offset;
        ControlFlow::Continue(())
    })?;
    assert_eq!(i, nr);

    let mut iter = BtreeIter::new(&trans, c::btree_id::extents, spos(0, u64::MAX, u32::MAX), BtreeIterFlags::empty());
    iter.for_each_reverse(&trans, POS_MIN, |k| {
        assert!(k.k.p.offset == i);
        i = k.k.start_offset();
        ControlFlow::Continue(())
    })?;
    assert_eq!(i, 0);
    Ok(())
}

fn test_iterate_slots(fs: &Fs, nr: u64) -> TestRet {
    delete_test_keys(fs)?;

    for i in 0..nr {
        let mut k = BkeyCookie::new();
        k.k_mut().p.offset = i * 2;
        k.k_mut().p.snapshot = u32::MAX;
        insert_cookie(fs, c::btree_id::xattrs, &mut k)?;
    }

    let trans = BtreeTrans::new(fs);
    let mut i = 0;
    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, 0, u32::MAX), BtreeIterFlags::empty());
    iter.for_each_max(&trans, pos(0, u64::MAX), |k| {
        assert!(k.k.p.offset == i);
        i += 2;
        ControlFlow::Continue(())
    })?;
    assert_eq!(i, nr * 2);

    i = 0;
    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, 0, u32::MAX), BtreeIterFlags::SLOTS);
    iter.for_each_max(&trans, pos(0, u64::MAX), |k| {
        if i >= nr * 2 {
            return ControlFlow::Break(());
        }
        assert!(k.k.p.offset == i);
        assert_eq!(k.k.is_deleted(), (i & 1) != 0);
        i += 1;
        ControlFlow::Continue(())
    })
}

fn test_iterate_slots_extents(fs: &Fs, nr: u64) -> TestRet {
    delete_test_keys(fs)?;

    for i in (0..nr).step_by(16) {
        let mut k = BkeyCookie::new();
        k.k_mut().p.offset = i + 16;
        k.k_mut().p.snapshot = u32::MAX;
        k.k_mut().size = 8;
        insert_cookie(fs, c::btree_id::extents, &mut k)?;
    }

    let trans = BtreeTrans::new(fs);
    let mut i = 0;
    let mut iter = BtreeIter::new(&trans, c::btree_id::extents, spos(0, 0, u32::MAX), BtreeIterFlags::empty());
    iter.for_each_max(&trans, pos(0, u64::MAX), |k| {
        assert_eq!(k.k.start_offset(), i + 8);
        assert_eq!({ k.k.size }, 8);
        i += 16;
        ControlFlow::Continue(())
    })?;
    assert_eq!(i, nr);

    i = 0;
    let mut iter = BtreeIter::new(&trans, c::btree_id::extents, spos(0, 0, u32::MAX), BtreeIterFlags::SLOTS);
    iter.for_each_max(&trans, pos(0, u64::MAX), |k| {
        if i == nr {
            return ControlFlow::Break(());
        }
        assert_eq!(k.k.is_deleted(), i % 16 == 0);
        assert_eq!(k.k.start_offset(), i);
        assert_eq!({ k.k.size }, 8);
        i = k.k.p.offset;
        ControlFlow::Continue(())
    })
}

fn test_peek_end_btree(fs: &Fs, btree: c::btree_id) -> TestRet {
    delete_test_keys(fs)?;

    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(&trans, btree, spos(0, 0, u32::MAX), BtreeIterFlags::empty());

    for _ in 0..2 {
        let empty = lockrestart_do(&trans, |t| {
            t.done(iter.peek_max(pos(0, u64::MAX))?.is_none())
        })?;
        assert!(empty);
    }

    Ok(())
}

fn test_peek_end(fs: &Fs, _nr: u64) -> TestRet {
    test_peek_end_btree(fs, c::btree_id::xattrs)
}

fn test_peek_end_extents(fs: &Fs, _nr: u64) -> TestRet {
    test_peek_end_btree(fs, c::btree_id::extents)
}

static TEST_VERSION: AtomicU64 = AtomicU64::new(0);

fn insert_test_extent(fs: &Fs, start: u64, end: u64) -> TestRet {
    let mut k = BkeyCookie::new();
    k.k_mut().p.offset = end;
    k.k_mut().p.snapshot = u32::MAX;
    k.k_mut().size = (end - start) as u32;
    k.k_mut().bversion.lo = TEST_VERSION.fetch_add(1, Ordering::Relaxed);
    insert_cookie(fs, c::btree_id::extents, &mut k)
}

fn __test_extent_overwrite(fs: &Fs, e1_start: u64, e1_end: u64, e2_start: u64, e2_end: u64) -> TestRet {
    insert_test_extent(fs, e1_start, e1_end)?;
    insert_test_extent(fs, e2_start, e2_end)?;
    delete_test_keys(fs)
}

fn test_extent_overwrite_front(fs: &Fs, _nr: u64) -> TestRet {
    __test_extent_overwrite(fs, 0, 64, 0, 32)?;
    __test_extent_overwrite(fs, 8, 64, 0, 32)
}

fn test_extent_overwrite_back(fs: &Fs, _nr: u64) -> TestRet {
    __test_extent_overwrite(fs, 0, 64, 32, 64)?;
    __test_extent_overwrite(fs, 0, 64, 32, 72)
}

fn test_extent_overwrite_middle(fs: &Fs, _nr: u64) -> TestRet {
    __test_extent_overwrite(fs, 0, 64, 32, 40)
}

fn test_extent_overwrite_all(fs: &Fs, _nr: u64) -> TestRet {
    __test_extent_overwrite(fs, 32, 64,  0,  64)?;
    __test_extent_overwrite(fs, 32, 64,  0, 128)?;
    __test_extent_overwrite(fs, 32, 64, 32,  64)?;
    __test_extent_overwrite(fs, 32, 64, 32, 128)
}

fn insert_test_overlapping_extent(fs: &Fs, inum: u64, start: u64, len: u32, snapid: u32) -> TestRet {
    let trans = BtreeTrans::new(fs);
    commit_do(&trans, None, NO_ENOSPC, |t| {
        let mut k = trans_cookie_alloc(&t)?;
        k.k_mut().p.inode = inum;
        k.k_mut().p.offset = start + len as u64;
        k.k_mut().p.snapshot = snapid;
        k.k_mut().size = len;
        t.insert_nonextent(c::btree_id::extents, k, INTERNAL_SNAPSHOT_NODE)
    })
}

fn test_extent_create_overlapping(fs: &Fs, inum: u64) -> TestRet {
    insert_test_overlapping_extent(fs, inum,  0, 16, u32::MAX - 2)?;
    insert_test_overlapping_extent(fs, inum,  2,  8, u32::MAX - 2)?;
    insert_test_overlapping_extent(fs, inum,  4,  4, u32::MAX)?;
    insert_test_overlapping_extent(fs, inum, 32,  8, u32::MAX - 2)?;
    insert_test_overlapping_extent(fs, inum, 36,  8, u32::MAX)?;
    insert_test_overlapping_extent(fs, inum, 60,  8, u32::MAX - 2)?;
    insert_test_overlapping_extent(fs, inum, 64,  8, u32::MAX)
}

fn test_extent_create_dup(fs: &Fs, inum: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(
        &trans,
        c::btree_id::extents,
        pos(inum, 0),
        BtreeIterFlags::ALL_SNAPSHOTS,
    );
    let res = DiskReservation::new(fs);

    let ret = commit_do(&trans, Some(&res), CommitOpts::new(), |t| {
        let fs = t.fs();
        let k = fs.require(iter.peek_max(pos(inum, u64::MAX))?, ENOENT_bkey_type_mismatch)?;
        fs.ensure(k.k.key_type() == c::bch_bkey_type::KEY_TYPE_extent, ENOENT_bkey_type_mismatch)?;

        let mut dup = t.bkey_make_mut_noupdate(k)?;
        let src_end = dup.k().p.offset;
        let size = dup.k().size;
        dup.k_mut().p.offset = round_up(src_end + 1024, 64) + size as u64;

        if size as u64 > res.sectors() {
            res.add(size as u64 - res.sectors(), c::bch_reservation_flags(0))?;
        }

        t.insert_nonextent(c::btree_id::extents, dup, INTERNAL_SNAPSHOT_NODE)
    });

    ret
}

fn test_btree_ptr_stale_dirty_key<'a, 't>(
    t: TransAttempt<'a, 't>,
    k: BkeySC<'_>,
) -> Result<TransAttempt<'a, 't>, TransError> {
    let mut iter = BtreeIter::new(
        t.trans(),
        c::btree_id::extents,
        k.k.p,
        BtreeIterFlags::INTENT,
    );

    let t = iter.traverse(t)?;
    let fs = t.fs();

    let b = fs.require(iter.node_at_iter_level(&t), ENOENT_bkey_type_mismatch)?;
    fs.ensure(b.level() == 0, ENOENT_bkey_type_mismatch)?;

    let mut update = t.bkey_make_mut_noupdate(b.key_sc())?;

    let mut updated = false;
    let mut update_k = BkeyS::from(update.as_mut());
    for ptr_entry in bkey_ptrs_mut(t.fs(), &mut update_k) {
        if ptr_entry.cached() == 0 && ptr_entry.dev() != c::BCH_SB_MEMBER_INVALID as u64 {
            ptr_entry.set_generation(ptr_entry.generation().wrapping_sub(1));
            updated = true;
            break;
        }
    }

    if updated {
        return t.btree_node_update_key(
            &mut iter,
            b,
            update,
            NO_ENOSPC,
            true
        );
    }

    fs.throw(ENOENT_bkey_type_mismatch)?
}

fn test_btree_ptr_stale_dirty(fs: &Fs, _nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);
    lockrestart_do(&trans, |t| {
        let mut iter = BtreeNodeIter::new(
            t.trans(),
            c::btree_id::extents,
            POS_MIN,
            0,
            1,
            BtreeIterFlags::INTENT,
        );

        let fs = t.fs();
        let k = fs.require(iter.peek_max_type(SPOS_MAX, BtreeIterFlags::empty())?, ENOENT_bkey_type_mismatch)?;
        fs.ensure(k.is_btree_ptr(), ENOENT_bkey_type_mismatch)?;

        test_btree_ptr_stale_dirty_key(t, k).map(|t| (t, ()))
    })
}

/// Error injector: corrupt one extent's stripe pointer so it no longer
/// matches its stripe, reproducing the "stripe pointer doesn't match stripe"
/// inconsistency (as seen in the field on d5cf3484) for iterating on the
/// erasure-coding repair path without a large real filesystem.
///
/// Walks the extents btree for the first extent carrying a stripe_ptr entry
/// and bumps that entry's block index. `bch2_ptr_matches_stripe()` then fails
/// against the (still-present, so the error names it) stripe, while the data
/// pointer's generation is left valid against its bucket -- so the mismatch
/// surfaces as the stripe inconsistency and not as a stale pointer.
///
/// The update runs with triggers disabled (`NORUN`) so the bad key is written
/// verbatim, exactly as an on-disk inconsistency fsck must later repair. The
/// caller must lay down erasure-coded data first (see ec.ktest); this errors
/// out if no striped extent is found.
fn test_inject_stripe_ptr_mismatch(fs: &Fs, _nr: u64) -> TestRet {
    const STRIPE_PTR: u32 = c::bch_extent_entry_type::BCH_EXTENT_ENTRY_stripe_ptr as u32;

    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(
        &trans,
        c::btree_id::extents,
        POS_MIN,
        BtreeIterFlags::INTENT | BtreeIterFlags::ALL_SNAPSHOTS,
    );

    loop {
        let outcome = lockrestart_do(&trans, |t| {
            let Some(k) = iter.peek_max(SPOS_MAX)? else {
                return t.done(Some(false));
            };

            let striped = bkey_extent_entries_sc(&k.v())
                .any(|e| extent_entry_type(e) == STRIPE_PTR);
            if !striped {
                return t.done(None);
            }

            let mut u = t.bkey_make_mut_noupdate(k)?;
            let mut u_k = BkeyS::from(u.as_mut());
            for entry in bkey_extent_entries_mut(t.fs(), &mut u_k) {
                if extent_entry_type(entry) == STRIPE_PTR {
                    let sp = entry_stripe_ptr_mut(entry);
                    sp.set_block(sp.block() + 1);
                    break;
                }
            }

            let t = t.update(&mut iter, u, UpdateTriggerFlags::NORUN)?;
            let t = t.commit(None, NO_ENOSPC)?;
            t.done(Some(true))
        })?;

        match outcome {
            Some(true) => return Ok(()),
            Some(false) => return fs.throw(ENOENT_bkey_type_mismatch),
            None => iter.advance(),
        }
    }
}

fn test_snapshot_filter(fs: &Fs, snapid_lo: u32, snapid_hi: u32) -> TestRet {
    let mut cookie = BkeyCookie::new();
    cookie.k_mut().p.snapshot = snapid_hi;
    insert_cookie(fs, c::btree_id::xattrs, &mut cookie)?;

    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, 0, snapid_lo), BtreeIterFlags::empty());

    let snapshot = lockrestart_do(&trans, |t| {
        t.done(iter.peek_max(pos(0, u64::MAX))?.map_or(0, |k| k.k.p.snapshot))
    })?;

    assert_eq!(snapshot, u32::MAX);
    Ok(())
}

fn test_snapshots(fs: &Fs, _nr: u64) -> TestRet {
    let mut cookie = BkeyCookie::new();
    cookie.k_mut().p.snapshot = u32::MAX;
    insert_cookie(fs, c::btree_id::xattrs, &mut cookie)?;

    let mut snapids = [0u32; 2];
    let snapid_subvols = [1u32, 1u32];

    trans_commit_do(fs, None, NO_ENOSPC, |t| {
        t.snapshot_node_create(u32::MAX, &mut snapids, &snapid_subvols)
    })?;

    if snapids[0] > snapids[1] {
        snapids.swap(0, 1);
    }

    test_snapshot_filter(fs, snapids[0], snapids[1])
}

fn test_rand() -> u64 {
    random_u64_below(u64::MAX)
}

fn rand_insert(fs: &Fs, nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);

    for _ in 0..nr {
        commit_do(&trans, None, NO_ENOSPC, |t| {
            let mut k = trans_cookie_alloc(&t)?;
            k.k_mut().p.offset = test_rand();
            k.k_mut().p.snapshot = u32::MAX;
            t.insert(c::btree_id::xattrs, k, UpdateTriggerFlags::empty())
        })?;
    }

    Ok(())
}

fn rand_insert_multi(fs: &Fs, nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);

    for _ in (0..nr).step_by(8) {
        commit_do(&trans, None, NO_ENOSPC, |mut t| {
            for _ in 0..8 {
                let mut k = trans_cookie_alloc(&t)?;
                k.k_mut().p.offset = test_rand();
                k.k_mut().p.snapshot = u32::MAX;
                t = t.insert(c::btree_id::xattrs, k, UpdateTriggerFlags::empty())?;
            }
            Ok(t)
        })?;
    }

    Ok(())
}

fn rand_lookup(fs: &Fs, nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, 0, u32::MAX), BtreeIterFlags::empty());

    for _ in 0..nr {
        iter.set_pos(spos(0, test_rand(), u32::MAX));
        lockrestart_do(&trans, |t| {
            iter.peek()?;
            t.done(())
        })?;
    }

    Ok(())
}

fn rand_mixed(fs: &Fs, nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, 0, u32::MAX), BtreeIterFlags::empty());

    for i in 0..nr {
        let rand = test_rand();
        commit_do(&trans, None, NO_ENOSPC, |mut t| {
            iter.set_pos(spos(0, rand, u32::MAX));

            let found = iter.peek()?.is_some();

            if (i & 3) == 0 && found {
                let mut cookie = trans_cookie_alloc(&t)?;
                cookie.k_mut().p = iter.pos();
                t = t.update(&mut iter, cookie, UpdateTriggerFlags::empty())?;
            }

            Ok(t)
        })?;
    }

    Ok(())
}

fn rand_delete(fs: &Fs, nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);

    for _ in 0..nr {
        let p = spos(0, test_rand(), u32::MAX);
        commit_do(&trans, None, NO_ENOSPC, |t| {
            t.delete(c::btree_id::xattrs, p, UpdateTriggerFlags::empty())
        })?;
    }

    Ok(())
}

fn seq_insert(fs: &Fs, nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);

    for i in 0..nr {
        commit_do(&trans, None, NO_ENOSPC, |t| {
            let mut insert = trans_cookie_alloc(&t)?;
            insert.k_mut().p = spos(0, i, u32::MAX);
            t.insert(c::btree_id::xattrs, insert, UpdateTriggerFlags::empty())
        })?;
    }

    Ok(())
}

fn seq_lookup(fs: &Fs, _nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, 0, u32::MAX), BtreeIterFlags::empty());
    iter.for_each_max(&trans, pos(0, u64::MAX), |_k| ControlFlow::Continue(()))
}

fn seq_overwrite(fs: &Fs, _nr: u64) -> TestRet {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::xattrs, spos(0, 0, u32::MAX), BtreeIterFlags::INTENT);

    loop {
        let done = lockrestart_do(&trans, |t| {
            let Some(k) = iter.peek()? else {
                return t.done(true);
            };
            let u = t.bkey_reassemble(k)?;
            let t = t.update(&mut iter, u, UpdateTriggerFlags::empty())?;
            let t = t.commit(None, NO_ENOSPC)?;
            t.done(false)
        })
        ?;

        if done {
            break;
        }
        iter.advance();
    }

    Ok(())
}

fn seq_delete(fs: &Fs, _nr: u64) -> TestRet {
    fs.btree_delete_range(
        c::btree_id::xattrs,
        spos(0, 0, u32::MAX),
        pos(0, u64::MAX),
        BtreeIterFlags::empty(),
    )
}

fn lookup_test(testname: &CStr) -> Option<(&'static [u8], TestFn)> {
    const TESTS: &[(&[u8], TestFn)] = &[
        (b"rand_insert", rand_insert),
        (b"rand_insert_multi", rand_insert_multi),
        (b"rand_lookup", rand_lookup),
        (b"rand_mixed", rand_mixed),
        (b"rand_delete", rand_delete),
        (b"seq_insert", seq_insert),
        (b"seq_lookup", seq_lookup),
        (b"seq_overwrite", seq_overwrite),
        (b"seq_delete", seq_delete),
        (b"test_delete", test_delete),
        (b"test_delete_written", test_delete_written),
        (b"test_iterate", test_iterate),
        (b"test_iterate_extents", test_iterate_extents),
        (b"test_iterate_slots", test_iterate_slots),
        (b"test_iterate_slots_extents", test_iterate_slots_extents),
        (b"test_peek_end", test_peek_end),
        (b"test_peek_end_extents", test_peek_end_extents),
        (b"test_extent_overwrite_front", test_extent_overwrite_front),
        (b"test_extent_overwrite_back", test_extent_overwrite_back),
        (b"test_extent_overwrite_middle", test_extent_overwrite_middle),
        (b"test_extent_overwrite_all", test_extent_overwrite_all),
        (b"test_extent_create_overlapping", test_extent_create_overlapping),
        (b"test_extent_create_dup", test_extent_create_dup),
        (b"test_btree_ptr_stale_dirty", test_btree_ptr_stale_dirty),
        (b"test_inject_stripe_ptr_mismatch", test_inject_stripe_ptr_mismatch),
        (b"test_snapshots", test_snapshots),
    ];

    TESTS.iter().copied().find(|(name, _)| testname.to_bytes() == *name)
}

struct TestJob {
    fs:         BorrowedFs,
    nr:         u64,
    nr_threads: u32,
    test:       TestFn,

    abort:      AtomicBool,
    ret:        AtomicI32,
}

impl TestJob {
    fn new(fs: &Fs, nr: u64, nr_threads: u32, test: TestFn) -> Result<Arc<Self>, BchError> {
        let job = TestJob {
            fs:         BorrowedFs::new(fs),
            nr,
            nr_threads,
            test,
            abort:      AtomicBool::new(false),
            ret:        AtomicI32::new(0),
        };

        // The only logic-level cfg left after the async conversion: kernel
        // `Arc::new` is fallible and demands a GFP context, userspace's isn't. If
        // kernel alloc inherited its context from `current` (scoped memalloc
        // flags) instead of threading GFP per call, even this would unify.
        #[cfg(kernel)]
        let job = Arc::new(job, GFP_KERNEL).map_err(|_| enomem())?;
        #[cfg(not(kernel))]
        let job = Arc::new(job);

        Ok(job)
    }
}

/// One worker: run the test (unless the batch aborted), report to `wg`.
async fn btree_perf_test_one(job: Arc<TestJob>, wg: Arc<WaitGroup>) {
    if !job.abort.load(Ordering::Acquire) {
        let fs = job.fs.get();
        if let Err(e) = (job.test)(&fs, job.nr / job.nr_threads as u64) {
            job.ret
                .compare_exchange(0, error_ret(e), Ordering::AcqRel, Ordering::Relaxed)
                .ok();
        }
    }

    wg.done();
}

fn print_perf_result(
    name:          &[u8],
    nr_threads:    u32,
    time:          u64,
    nsec_per_iter: u64,
    nr_buf:        &Printbuf,
    per_sec_buf:   &Printbuf,
) {
    let name = core::str::from_utf8(name).unwrap_or("<invalid>");
    let secs = time / c::NSEC_PER_SEC as u64;

    kernel::pr_info!(
        "{}: {} with {} threads in {} sec, {} nsec per iter, {} per sec\n",
        name,
        nr_buf.as_str(),
        nr_threads,
        secs,
        nsec_per_iter,
        per_sec_buf.as_str(),
    );
}

#[no_mangle]
pub unsafe extern "C" fn bch2_btree_perf_test(
    raw_fs:     *mut c::bch_fs,
    testname:   *const c_char,
    nr:         u64,
    nr_threads: u32,
) -> c_int {
    let fs = unsafe { Fs::borrow_raw(raw_fs) };

    if nr == 0 || nr_threads == 0 {
        return errcode(bch_errcode::BCH_ERR_EINVAL_test_zero_nr_or_threads);
    }

    let Some(testname) = (!testname.is_null()).then(|| unsafe { CStr::from_ptr(testname) }) else {
        return errcode(bch_errcode::BCH_ERR_EINVAL_test_unknown_test);
    };

    let Some((name, test)) = lookup_test(testname) else {
        return errcode(bch_errcode::BCH_ERR_EINVAL_test_unknown_test);
    };

    let job = match TestJob::new(&*fs, nr, nr_threads, test) {
        Ok(job) => job,
        Err(e) => return error_ret(e),
    };

    let queue = system_unbound();
    let wg = match WaitGroup::new(nr_threads) {
        Ok(wg) => wg,
        Err(_) => return error_ret(enomem()),
    };

    // Fork the workers and wait for all of them: `block_on` drives the join on
    // this thread, parking until the last worker reports in, and hands back the
    // shared timing/result so nothing has to outlive the moved `job`. `None` means
    // a worker failed to spawn (the start was never taken, so we can't time it).
    let outcome = block_on(async move {
        let mut spawned = 0;
        for _ in 0..nr_threads {
            if spawn(queue, btree_perf_test_one(job.clone(), wg.clone())).is_err() {
                break;
            }
            spawned += 1;
        }

        // A short spawn can't be timed (the missing workers never run), so flag
        // the batch aborted and balance the group for the ones that won't report.
        if spawned != nr_threads {
            job.abort.store(true, Ordering::Release);
            for _ in spawned..nr_threads {
                wg.done();
            }
        }

        // Start the clock once the workers are launched; they run as the queue
        // schedules them, and block_on parks here until the last reports in.
        let start = local_clock();
        wg.wait().await;

        (spawned == nr_threads).then(|| (start, job.ret.load(Ordering::Acquire)))
    });

    let Some((start, ret_code)) = outcome else {
        return error_ret(enomem());
    };

    let finish = local_clock();
    let time = finish - start;

    let mut nr_buf = Printbuf::new();
    let mut per_sec_buf = Printbuf::new();
    nr_buf.human_readable_u64(nr);
    per_sec_buf.human_readable_u64(if time != 0 {
        // nr is caller-supplied — for the unit tests it's an inode number, not an op
        // count — so nr * NSEC_PER_SEC overflows u64 (C wrapped it silently; the debug
        // build BUGs on the overflow check). saturating_mul caps instead: real perf
        // runs (sane nr) stay exact, the unit tests get a bounded-nonsense number.
        // NOT u128 — 128-bit division pulls in __udivti3, which the kernel lacks.
        nr.saturating_mul(c::NSEC_PER_SEC as u64) / time
    } else {
        0
    });

    let nsec_per_iter = if nr != 0 {
        (time * nr_threads as u64) / nr
    } else {
        0
    };

    print_perf_result(name, nr_threads, time, nsec_per_iter, &nr_buf, &per_sec_buf);

    ret_code
}
