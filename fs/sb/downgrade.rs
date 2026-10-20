// SPDX-License-Identifier: GPL-2.0

//! The superblock's downgrade section: for each version a downgrade goes back
//! past, the recovery passes to run and the errors to fix silently.
//!
//! It's written by the version that knows what downgrading from it costs, for
//! an older one to read - bch2_sb_set_downgrade(), when an older version
//! mounts a newer filesystem.

use core::ffi::{c_int, c_uint, c_ulong};
use core::mem::size_of;
use core::ptr::NonNull;

use cstruct_macros::c_export;

use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::init::passes;
use crate::prt;
use crate::sb::errors::prt_error_ids;
use crate::sb::io::{entries_end, SbFieldOps};
use crate::types::Le64;
use crate::util::alloc::{flags::GFP_KERNEL, AllocError, KVVec};
use crate::util::bitmap::LeBitmap;
use crate::util::ffi::Opaque;
use crate::util::vstructs::{bytes_of_mut, Head, Validated, ValidatedEntries};
use crate::util::Printbuf;

type Section = c::bch_sb_field_downgrade;
type Entry = c::bch_sb_field_downgrade_entry<Validated>;

const SECTION_HEAD: usize = size_of::<Head<Section>>();

/// Its whole entries. Each is 2 byte aligned and the section's length a
/// multiple of 8, so up to 6 bytes past the last are padding.
fn entries(f: &Section) -> ValidatedEntries<'_, Entry> {
    ValidatedEntries::new(&f.as_bytes()[SECTION_HEAD..])
}

impl SbFieldOps for Section {
    fn validate(sb: &c::bch_sb, f: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
        let sb_major = c::BCH_VERSION_MAJOR(sb.version.get().into());
        for e in entries(f) {
            let major = c::BCH_VERSION_MAJOR(e.version.get().into());
            if major != sb_major {
                write!(err, "downgrade entry with mismatched major version ({major} != {sb_major})");
                return Err(bch_errcode::BCH_ERR_invalid_sb_downgrade.into());
            }
        }
        Ok(())
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        if out.nr_tabstops() <= 1 {
            out.tabstop_push(16);
        }

        for e in entries(f) {
            prt!(out, "version:\t{}\n", c::bcachefs_metadata_version(e.version.get().into()));

            write!(out, "recovery passes:\t");
            passes::prt_stable(out, e.recovery_passes[0].get());
            out.newline();

            write!(out, "errors:\t");
            prt_error_ids(out, e.errors.iter().map(|id| c::bch_sb_error_id(id.get().into())));
            out.newline();
        }
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<Entry>(b, SECTION_HEAD, |_| true)
    }
}

// ── The upgrade and downgrade tables ─────────────────────────────────────────

/// What upgrading to - or downgrading from - @version needs: recovery passes
/// to run, and errors they'll find that are to be fixed silently.
struct UpDown {
    version:  c::bcachefs_metadata_version,
    /// Every fsck pass, as well as @passes.
    all_fsck: bool,
    /// BCH_RECOVERY_PASS_* bits.
    passes:   u64,
    errors:   &'static [c::bch_sb_error_id],
}

/// `version => [passes] (+ all_fsck), [errors];`
macro_rules! table {
    ($( $ver:ident => [$($pass:ident),*] $(+ $all:ident)?, [$($err:ident),*]; )*) => { ::paste::paste! {
        &[$(UpDown {
            version:  c::bcachefs_metadata_version::[<bcachefs_metadata_version_ $ver>],
            all_fsck: false $(|| table!(@$all))?,
            passes:   0 $(| 1 << c::bch_recovery_pass::[<BCH_RECOVERY_PASS_ $pass>] as u64)*,
            errors:   &[$(c::bch_sb_error_id::[<BCH_FSCK_ERR_ $err>]),*],
        }),*]
    } };
    (@all_fsck) => { true };
}

const UPGRADE: &[UpDown] = table! {
    snapshot_2                  => [] + all_fsck, [subvol_root_wrong_bi_subvol, subvol_not_master_and_not_snapshot];
    backpointers                => [] + all_fsck, [];
    inode_v3                    => [] + all_fsck, [];
    unwritten_extents           => [] + all_fsck, [];
    bucket_gens                 => [bucket_gens_init] + all_fsck, [];
    lru_v2                      => [] + all_fsck, [];
    fragmentation_lru           => [] + all_fsck, [];
    no_bps_in_alloc_keys        => [] + all_fsck, [];
    snapshot_trees              => [] + all_fsck, [];
    snapshot_skiplists          => [check_snapshots], [snapshot_bad_depth, snapshot_bad_skiplist];
    deleted_inodes              => [check_inodes], [unlinked_inode_not_on_deleted_list];
    rebalance_work              => [set_fs_needs_reconcile], [];
    subvolume_fs_parent         => [check_dirents], [subvol_fs_path_parent_wrong];
    btree_subvolume_children    => [check_subvols], [subvol_children_not_set];
    mi_btree_bitmap             => [check_allocations], [btree_bitmap_not_marked];
    disk_accounting_v2          => [check_allocations],
        [bkey_version_in_future, dev_usage_buckets_wrong, dev_usage_sectors_wrong,
         dev_usage_fragmented_wrong, accounting_mismatch];
    disk_accounting_v3          => [check_allocations],
        [bkey_version_in_future, dev_usage_buckets_wrong, dev_usage_sectors_wrong,
         dev_usage_fragmented_wrong, accounting_mismatch, accounting_key_replicas_nr_devs_0,
         accounting_key_replicas_nr_required_bad, accounting_key_replicas_devs_unsorted,
         accounting_key_junk_at_end];
    disk_accounting_inum        => [check_allocations], [accounting_mismatch];
    rebalance_work_acct_fix     => [check_allocations], [accounting_mismatch];
    inode_has_child_snapshots   => [check_inodes], [inode_has_child_snapshots_wrong];
    backpointer_bucket_gen      => [check_extents_to_backpointers],
        [backpointer_to_missing_ptr, ptr_to_missing_backpointer];
    disk_accounting_big_endian  => [check_allocations],
        [accounting_mismatch, accounting_key_replicas_nr_devs_0, accounting_key_junk_at_end];
    cached_backpointers         => [check_extents_to_backpointers], [ptr_to_missing_backpointer];
    stripe_backpointers         => [check_extents_to_backpointers], [ptr_to_missing_backpointer];
    inode_has_case_insensitive  => [check_inodes],
        [inode_has_case_insensitive_not_set, inode_parent_has_case_insensitive_not_set];
    btree_node_accounting       => [check_allocations], [accounting_mismatch];
    reconcile                   => [check_reconcile_work], [accounting_mismatch, extent_io_opts_not_set];
    bucket_stripe_index         => [check_alloc_info, check_alloc_to_lru_refs],
        [alloc_key_stripe_refcount_wrong, stripe_to_missing_bucket_ref];
    need_discard_by_journal_seq => [check_alloc_info],
        [need_discard_freespace_key_bad, need_discard_key_wrong,
         need_discard_freespace_key_to_invalid_dev_bucket];
    per_dev_fragmentation_lru   => [check_allocations, check_lrus, check_alloc_to_lru_refs, check_inodes,
                                    check_xattrs, check_snapshots, check_subvols, delete_dead_snapshots],
        [accounting_mismatch, lru_entry_bad, alloc_key_to_missing_lru_entry,
         inode_has_inode_opts_flag_wrong, inode_has_access_acl_flag_wrong,
         inode_has_default_acl_flag_wrong, snapshot_state_bad, subvol_state_bad];
};

/// For an incompatible upgrade: one past what version_incompat_allowed was.
const UPGRADE_INCOMPAT: &[UpDown] = table! {
    reconcile => [check_reconcile_work], [extent_io_opts_not_set];
};

const DOWNGRADE: &[UpDown] = table! {
    bucket_stripe_sectors       => [], [];
    disk_accounting_v2          => [check_allocations],
        [dev_usage_buckets_wrong, dev_usage_sectors_wrong, dev_usage_fragmented_wrong,
         fs_usage_hidden_wrong, fs_usage_btree_wrong, fs_usage_data_wrong, fs_usage_cached_wrong,
         fs_usage_reserved_wrong, fs_usage_nr_inodes_wrong, fs_usage_persistent_reserved_wrong,
         fs_usage_replicas_wrong, bkey_version_in_future];
    disk_accounting_v3          => [check_allocations],
        [dev_usage_buckets_wrong, dev_usage_sectors_wrong, dev_usage_fragmented_wrong,
         fs_usage_hidden_wrong, fs_usage_btree_wrong, fs_usage_data_wrong, fs_usage_cached_wrong,
         fs_usage_reserved_wrong, fs_usage_nr_inodes_wrong, fs_usage_persistent_reserved_wrong,
         fs_usage_replicas_wrong, accounting_replicas_not_marked, bkey_version_in_future];
    rebalance_work_acct_fix     => [check_allocations],
        [accounting_mismatch, accounting_key_replicas_nr_devs_0, accounting_key_junk_at_end];
    backpointer_bucket_gen      => [check_extents_to_backpointers],
        [backpointer_bucket_offset_wrong, backpointer_to_missing_ptr, ptr_to_missing_backpointer];
    disk_accounting_big_endian  => [check_allocations],
        [accounting_mismatch, accounting_key_replicas_nr_devs_0, accounting_key_junk_at_end];
    btree_node_accounting       => [check_allocations], [accounting_mismatch, accounting_key_nr_counters_wrong];
    bucket_stripe_index         => [check_alloc_info, check_alloc_to_lru_refs], [];
    need_discard_by_journal_seq => [check_alloc_info],
        [need_discard_freespace_key_bad, need_discard_key_wrong,
         need_discard_freespace_key_to_invalid_dev_bucket];
    per_dev_fragmentation_lru   => [check_allocations, check_lrus, check_alloc_to_lru_refs],
        [lru_entry_bad, alloc_key_to_missing_lru_entry, accounting_mismatch];
};

// ── Applying them ────────────────────────────────────────────────────────────

/// Every superblock has one from mount on.
fn ext(sb: &mut c::bch_sb_handle) -> &mut c::bch_sb_field_ext<Validated> {
    sb.field_mut().expect("an ext section")
}

/// Upgrading from @old to @new: what each version past @old, up to @new,
/// needs, into the ext section's recovery_passes_required and errors_silent.
fn set_upgrade(sb: &mut c::bch_sb_handle, table: &[UpDown], old: u32, new: u32) {
    let ext = ext(sb);
    for i in table.iter().filter(|i| old < i.version.0 && i.version.0 <= new) {
        let fsck = if i.all_fsck { passes::fsck_passes() } else { 0 };
        let passes = passes::passes_to_stable(i.passes | fsck);
        ext.recovery_passes_required[0] |= passes;
        for e in i.errors {
            LeBitmap::from_words_mut(&mut ext.errors_silent).set(e.0 as usize);
        }
    }
}

#[c_export]
pub fn bch2_sb_set_upgrade(c: &Opaque<c::bch_fs>, old: c_uint, new: c_uint) {
    // SAFETY: C calls it under sb_lock
    set_upgrade(&mut *unsafe { Fs::from_c(c).sb_lock_held() }, UPGRADE, old, new)
}

/// set_upgrade(), by the table for allowing incompatible features.
pub(crate) fn set_upgrade_incompat(sb: &mut c::bch_sb_handle, old: u32, new: u32) {
    set_upgrade(sb, UPGRADE_INCOMPAT, old, new)
}

impl Fs {
    /// Whether there's a stripes btree: a root read, not a fake one.
    fn have_stripes(&self) -> bool {
        // NULL, or the root: a failed read is the root's error, not its node
        let b = NonNull::new(self.btree.cache.roots_known[c::btree_id::BTREE_ID_stripes.0 as usize].b);
        // SAFETY: a root node, live while it's the root
        b.is_some_and(|b| !unsafe { b.as_ref() }.is_fake())
    }

    /// What an upgrade past bucket_stripe_sectors needs on a filesystem with
    /// stripes, which the tables can't say: check_allocations, and the bucket
    /// sector counts it fixes silently.
    pub fn sb_set_upgrade_extra(&self) {
        let mut sb = self.sb_lock();
        let v = c::bcachefs_metadata_version::bcachefs_metadata_version_bucket_stripe_sectors.0;
        let new = u32::from(self.sb.version);
        let old = match self.sb.version_upgrade_complete {
            0 => new,
            v => u32::from(v),
        };
        if !(old < v && v <= new && self.have_stripes()) {
            return;
        }

        let ext = ext(&mut sb);
        LeBitmap::from_words_mut(&mut ext.recovery_passes_required)
            .set(c::bch_recovery_pass_stable::BCH_RECOVERY_PASS_STABLE_check_allocations.0 as usize);
        let silent = LeBitmap::from_words_mut(&mut ext.errors_silent);
        silent.set(c::bch_sb_error_id::BCH_FSCK_ERR_alloc_key_dirty_sectors_wrong.0 as usize);
        silent.set(c::bch_sb_error_id::BCH_FSCK_ERR_alloc_key_stripe_sectors_wrong.0 as usize);
        self.write_super(&mut sb);
    }
}

#[c_export]
pub fn bch2_sb_set_upgrade_extra(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).sb_set_upgrade_extra()
}

/// Downgrading from minor version @old_minor to @new_minor: what the section
/// says each minor version past @new_minor, up to @old_minor, needs - into
/// the ext section, and errors_silent in memory, which recovery reads.
#[c_export]
pub fn bch2_sb_set_downgrade(c: &Opaque<c::bch_fs>, new_minor: c_uint, old_minor: c_uint) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    let mut sb = unsafe { fs.sb_lock_held() };
    let Some(d) = sb.field::<Section>() else { return };

    let mut passes = [0; 2];
    let mut silent = [Le64::new(0); 8];
    for e in entries(d) {
        let minor = c::BCH_VERSION_MINOR(e.version.get().into()) as u32;
        if !(new_minor < minor && minor <= old_minor) {
            continue;
        }

        passes[0] |= e.recovery_passes[0].get();
        passes[1] |= e.recovery_passes[1].get();
        for id in e.errors.iter().map(|id| usize::from(id.get())) {
            if id < c::BCH_FSCK_ERR_MAX as usize {
                let bits = c_ulong::BITS as usize;
                // SAFETY: c->sb, which its writers change under sb_lock
                unsafe { (*fs.as_ptr()).sb.errors_silent[id / bits] |= 1 << (id % bits) };
            }
            if id < silent.len() * 64 {
                LeBitmap::from_words_mut(&mut silent).set(id);
            }
        }
    }

    let ext = ext(&mut sb);
    for (w, p) in ext.recovery_passes_required.iter_mut().zip(passes) {
        *w |= p;
    }
    for (w, s) in ext.errors_silent.iter_mut().zip(silent) {
        *w |= s.get();
    }
}

/// The downgrade section's entries, from the downgrade table: for each
/// version of @major from @version_incompat on, what downgrading past it
/// needs - those that need nothing left out.
fn downgrade_entries(major: u64, version_incompat: u32, have_stripes: bool) -> Result<KVVec<u8>, AllocError> {
    let mut table = KVVec::<u8>::new();
    for src in DOWNGRADE.iter().filter(|s| {
        c::BCH_VERSION_MAJOR(s.version.0.into()) == major && s.version.0 >= version_incompat
    }) {
        let mut passes = passes::passes_to_stable(src.passes);
        let mut nr_errors = src.errors.len();
        // what the table can't say: on a filesystem with stripes, downgrading
        // past bucket_stripe_sectors needs check_allocations, and the bucket
        // sector counts it fixes silently
        let extra = have_stripes &&
            src.version == c::bcachefs_metadata_version::bcachefs_metadata_version_bucket_stripe_sectors;
        if extra {
            passes |= 1 << c::bch_recovery_pass_stable::BCH_RECOVERY_PASS_STABLE_check_allocations.0;
            nr_errors += 1;
        }
        if passes == 0 && nr_errors == 0 {
            continue;
        }

        let errors = src.errors.iter()
            .chain(extra.then_some(&c::bch_sb_error_id::BCH_FSCK_ERR_alloc_key_dirty_sectors_wrong));
        table.extend_from_slice(&(src.version.0 as u16).to_le_bytes(), GFP_KERNEL)?;
        table.extend_from_slice(&passes.to_le_bytes(), GFP_KERNEL)?;
        table.extend_from_slice(&0u64.to_le_bytes(), GFP_KERNEL)?;
        table.extend_from_slice(&(nr_errors as u16).to_le_bytes(), GFP_KERNEL)?;
        for e in errors {
            table.extend_from_slice(&(e.0 as u16).to_le_bytes(), GFP_KERNEL)?;
        }
    }
    Ok(table)
}

impl Fs {
    /// The downgrade section, from downgrade_entries() - for a superblock
    /// write.
    fn sb_downgrade_update(&self, sb: &mut SbLockGuard) -> Result<(), BchError> {
        // have_stripes() reads the btree roots
        if !self.flag(c::bch_fs_flags::BCH_FS_btree_running) {
            return Ok(());
        }

        let table = downgrade_entries(c::BCH_VERSION_MAJOR(sb.sb().version.get().into()),
                                      self.sb.version_incompat.into(), self.have_stripes())?;

        let bytes = (SECTION_HEAD + table.len()).next_multiple_of(size_of::<u64>());

        // Larger than ours, it's a newer version's: what downgrading from
        // that version costs, which only it knows and an older version reads
        // when it's downgraded to. Rewriting it from our smaller table would
        // lose the entries for the versions past ours - so a table of ours
        // that shrank isn't written either, until the section is gone.
        if sb.field::<Section>().is_some_and(|d| d.as_bytes().len() > bytes) {
            return Ok(());
        }

        let d = sb.field_resize::<Section>((bytes / size_of::<u64>()) as u32)
            .ok_or_else(|| self.err(bch_errcode::BCH_ERR_ENOSPC_sb_downgrade))?;
        let d = &mut bytes_of_mut(d)[SECTION_HEAD..];
        d[..table.len()].copy_from_slice(&table);
        d[table.len()..].fill(0);
        Ok(())
    }
}

/// For a superblock write.
#[c_export]
pub fn bch2_sb_downgrade_update(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock, from bch2_write_super()
    ret_to_c(fs.sb_downgrade_update(&mut unsafe { fs.sb_lock_held() }))
}
