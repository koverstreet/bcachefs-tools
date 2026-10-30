// SPDX-License-Identifier: GPL-2.0

//! Recovery - init/recovery.c's, as it converts. So far what it records in
//! the superblock: a btree that lost data and the repairs that takes, alloc
//! info dropped to be reconstructed, and the errors a journal rewind leaves
//! expected. Each is idempotent, so what it dirties is exactly what's new.

use crate::btree::bkey::{POS_MIN, SPOS_MAX};
use crate::btree::journal_overlay::shoot_down_journal_keys;
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::{btree_id_is_alloc, Fs};
use crate::init::error::id;
use crate::init::passes::{self, run_explicit_sb};
use crate::prt;
use crate::sb::io::SbWrite;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use core::ffi::c_int;
use cstruct_macros::c_export;

type Pass = c::bch_recovery_pass;

macro_rules! passes {
    ($($p:ident),* $(,)?) => { ::paste::paste! { &[$(Pass::[<BCH_RECOVERY_PASS_ $p>]),*] } };
}

/// What repairs btree @btree having lost data, besides check_topology and
/// check_allocations, which every btree's loss needs. A content btree's keys
/// are checked against what refers to them, and what they refer to: damage to
/// one leaves dangling references in the others, which otherwise surface later
/// as runtime errors - a dirents btree flagged lost in one boot is a dirent to
/// a missing inode weeks after.
fn lost_data_passes(btree: c::btree_id) -> &'static [Pass] {
    use c::btree_id as b;

    match btree {
        b::BTREE_ID_alloc |
        b::BTREE_ID_need_discard |
        b::BTREE_ID_freespace |
        b::BTREE_ID_bucket_gens |
        b::BTREE_ID_lru                 => passes![check_alloc_info],
        b::BTREE_ID_backpointers        => passes![check_btree_backpointers, check_extents_to_backpointers],
        b::BTREE_ID_accounting          => passes![check_allocations],
        b::BTREE_ID_snapshots           => passes![reconstruct_snapshots, check_topology, scan_for_btree_nodes],
        b::BTREE_ID_snapshot_trees      => passes![check_snapshot_trees, check_snapshots],
        b::BTREE_ID_extents             => passes![check_extents, check_inodes, check_extents_to_backpointers],
        b::BTREE_ID_inodes              => passes![check_inodes, check_extents, check_dirents, check_xattrs,
                                                   check_nlinks],
        b::BTREE_ID_dirents             => passes![check_dirents, check_inodes, check_nlinks,
                                                   check_directory_structure],
        b::BTREE_ID_xattrs              => passes![check_xattrs],
        b::BTREE_ID_reflink             => passes![check_indirect_extents, check_extents],
        b::BTREE_ID_subvolumes          => passes![check_subvols, check_subvol_children,
                                                   check_subvolume_structure, check_root],
        b::BTREE_ID_subvolume_children  => passes![check_subvol_children, check_subvols],
        b::BTREE_ID_deleted_inodes      => passes![check_inodes],
        /* check_allocations re-derives bucket state from the stripes: */
        b::BTREE_ID_stripes |
        b::BTREE_ID_bucket_to_stripe |
        b::BTREE_ID_stripe_backpointers => passes![check_extents_to_backpointers],
        _                               => passes![check_topology, scan_for_btree_nodes],
    }
}

/// Btree @btree lost data: flagged - and in the forensic record, never cleared -
/// with its repairs scheduled and the errors they'll find, which the loss
/// explains, silenced. @msg is the report: unsuppressed if any of this was new,
/// so damage already reported doesn't print on every read of a bad node.
/// bch2_btree_lost_data().
pub fn btree_lost_data(fs: &Fs, msg: &mut Printbuf, btree: c::btree_id) -> Result<(), BchError> {
    let mut w = fs.sb_write();

    w.set_btrees_lost_data_ever(btree);
    if !fs.btree_lost_data(btree) {
        prt!(msg, "flagging btree {btree} lost data\n");
        w.set_btrees_lost_data(btree);
    }

    /* The last error, as C's ?: chain kept it: every pass is scheduled */
    let mut ret = Ok(());
    let mut run = |w: &mut SbWrite<'_>, pass| {
        if let Err(e) = run_explicit_sb(fs, msg, pass, c::bch_run_recovery_pass_flags::empty(), Some(w)) {
            ret = Err(e);
        }
    };

    /* Once we have runtime self healing for topology errors we won't need this: */
    run(&mut w, Pass::BCH_RECOVERY_PASS_check_topology);

    /* Btree node accounting will be off: */
    w.set_err_silent(id::accounting_mismatch);
    run(&mut w, Pass::BCH_RECOVERY_PASS_check_allocations);

    /* Minor, and not urgent, but debug wants the next fsck clean: */
    if cfg!(CONFIG_BCACHEFS_DEBUG) {
        run(&mut w, Pass::BCH_RECOVERY_PASS_check_lrus);
    }

    for e in [id::lru_entry_bad, id::alloc_key_to_missing_lru_entry, id::backpointer_to_missing_ptr,
              id::alloc_key_data_type_wrong, id::alloc_key_dirty_sectors_wrong, id::need_discard_key_wrong,
              id::freespace_key_wrong, id::reconcile_work_phys_incorrectly_set,
              id::reconcile_work_incorrectly_set] {
        w.set_err_silent(e);
    }
    if btree == c::btree_id::BTREE_ID_alloc {
        for e in [id::alloc_key_gen_wrong, id::alloc_key_cached_sectors_wrong, id::alloc_key_stripe_wrong,
                  id::alloc_key_stripe_redundancy_wrong] {
            w.set_err_silent(e);
        }
    }

    for &pass in lost_data_passes(btree) {
        run(&mut w, pass);
    }

    if w.is_dirty() {
        msg.set_suppressed(false);
    }
    ret
}

#[c_export]
pub fn bch2_btree_lost_data(c: &Opaque<c::bch_fs>, msg: &mut Printbuf, btree: c::btree_id) -> c_int {
    ret_to_c(btree_lost_data(Fs::from_c(c), msg, btree))
}

/// The alloc info dropped, to be rebuilt: the passes that rebuild it required,
/// the errors they'll find silenced, and the alloc btrees killed. For
/// -o reconstruct_alloc, or a filesystem with no alloc info going read-write:
/// bch2_reconstruct_alloc().
fn reconstruct_alloc(fs: &Fs) {
    {
        let mut w = fs.sb_write();

        let required = &mut w.ext_mut().recovery_passes_required[0];
        let mut v = required.get();
        for &pass in passes![check_allocations, check_alloc_info, check_lrus, check_extents_to_backpointers,
                            check_alloc_to_lru_refs] {
            v |= 1 << pass.to_stable().0;
        }
        required.set(v);

        for e in [id::ptr_to_missing_alloc_key, id::ptr_gen_newer_than_bucket_gen, id::stale_dirty_ptr,
                  id::dev_usage_buckets_wrong, id::dev_usage_sectors_wrong, id::dev_usage_fragmented_wrong,
                  id::fs_usage_btree_wrong, id::fs_usage_cached_wrong, id::fs_usage_persistent_reserved_wrong,
                  id::fs_usage_replicas_wrong, id::alloc_key_to_missing_lru_entry,
                  id::alloc_key_data_type_wrong, id::alloc_key_gen_wrong, id::alloc_key_dirty_sectors_wrong,
                  id::alloc_key_cached_sectors_wrong, id::alloc_key_stripe_wrong,
                  id::alloc_key_stripe_redundancy_wrong, id::need_discard_key_wrong, id::freespace_key_wrong,
                  id::bucket_gens_key_wrong, id::freespace_hole_missing, id::ptr_to_missing_backpointer,
                  id::lru_entry_bad, id::accounting_mismatch] {
            w.set_err_silent(e);
        }

        let compat = &w.published().compat;
        compat.set(compat.get() & !(1 << c::bch_sb_compat::BCH_COMPAT_alloc_info.0));

        let opts = fs.opts_writer(&mut w);
        opts.recovery_passes.set(opts.recovery_passes.get() | passes::passes_from_stable(v));

        let features = &mut w.sb_mut().features[0];
        features.set(features.get() & !(1 << c::bch_sb_feature::BCH_FEATURE_no_alloc_info.0));
        w.dirty();
    }

    for btree in (0..fs.btree_id_nr_alive()).filter(|&b| btree_id_is_alloc(b)) {
        fs.btree.cache.roots.lock().get_mut(btree).expect("a btree below btree_id_nr_alive()").alive = 0;
        shoot_down_journal_keys(fs, c::btree_id(btree), 0, c::BTREE_MAX_DEPTH, POS_MIN, SPOS_MAX);
    }
}

#[c_export]
pub fn bch2_reconstruct_alloc(c: &Opaque<c::bch_fs>) {
    reconstruct_alloc(Fs::from_c(c))
}

/// After a journal rewind, the alloc info is stale for buckets whose state
/// changed between the rewind point and the original journal head: what
/// check_alloc_info finds of that, silenced. bch2_ignore_journal_rewind_errors().
fn ignore_journal_rewind_errors(fs: &Fs) {
    let mut w = fs.sb_write();
    for e in [id::alloc_key_data_type_wrong, id::alloc_key_gen_wrong, id::alloc_key_dirty_sectors_wrong,
              id::alloc_key_cached_sectors_wrong, id::alloc_key_stripe_wrong,
              id::alloc_key_stripe_redundancy_wrong, id::need_discard_key_wrong, id::freespace_key_wrong,
              id::freespace_hole_missing, id::bucket_gens_key_wrong, id::alloc_key_to_missing_lru_entry,
              id::lru_entry_bad, id::accounting_mismatch, id::backpointer_to_missing_ptr] {
        w.set_err_silent(e);
    }
    w.dirty();
}

#[c_export]
pub fn bch2_ignore_journal_rewind_errors(c: &Opaque<c::bch_fs>) {
    ignore_journal_rewind_errors(Fs::from_c(c))
}
