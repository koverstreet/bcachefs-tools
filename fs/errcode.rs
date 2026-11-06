//! bcachefs error codes.
//!
//! The errcodes extend the errnos: BCH_ERRCODES() numbers bcachefs's own from
//! BCH_ERR_START (2048) up, each with a parent - an errno, another errcode, or
//! none. A code is thus a chain: ENOENT_inode -> ENOENT, and
//! transaction_restart_relock -> transaction_restart. matches(class) asks
//! whether @class is on the chain, errno() is its root: what the VFS and
//! userspace see.
//!
//! ERRCODES, generated from BCH_ERRCODES(), holds each errcode's name and
//! parent; errcode.c keeps C's own copy for its C callers. Parents are written
//! as an errno or BCH_ERR_<name>, and `class` resolves both to numbers - the
//! errnos from the kernel's bindings, or linux-raw-sys's in userspace. An
//! errno's name is errname()'s, strerror()'s in userspace.
//!
//! A number with no entry has no parent and stands for itself. That includes
//! codes past BCH_ERR_MAX: the tools and the DKMS module are versioned
//! separately, and mount.bcachefs acts on the errcode a syscall returned, so a
//! newer module can return one this build doesn't know.

#![allow(non_camel_case_types)]

use crate::c;
use core::ffi::{c_int, CStr};
use core::fmt;
use core::iter;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI32, Ordering};
use crate::types::c_default;
use cstruct_macros::{CStruct, c_enum, c_export, c_extern, c_xmacro, TypeInfo};

c_xmacro! {
    BLK_ERRS(BLK_STS) {
        (NOTSUPP, 1),
        (TIMEOUT, 2),
        (NOSPC, 3),
        (TRANSPORT, 4),
        (TARGET, 5),
        (RESV_CONFLICT, 6),
        (MEDIUM, 7),
        (PROTECTION, 8),
        (RESOURCE, 9),
        (IOERR, 10),
        (DM_REQUEUE, 11),
        (AGAIN, 12),
        (DEV_RESOURCE, 13),
        (ZONE_OPEN_RESOURCE, 14),
        (ZONE_ACTIVE_RESOURCE, 15),
        (OFFLINE, 16),
        (DURATION_LIMIT, 17),
        (INVAL, 18),
        (REMOVED, 19),
    }
}

c_xmacro! {
    ZSTD_ERRS(ZSTD_error) {
        (GENERIC, 508),
        (prefix_unknown, 509),
        (version_unsupported, 510),
        (frameParameter_unsupported, 511),
        (frameParameter_windowTooLarge, 512),
        (corruption_detected, 513),
        (checksum_wrong, 514),
        (dictionary_corrupted, 515),
        (dictionary_wrong, 516),
        (dictionaryCreation_failed, 517),
        (parameter_unsupported, 518),
        (parameter_outOfBound, 519),
        (tableLog_tooLarge, 520),
        (maxSymbolValue_tooLarge, 521),
        (maxSymbolValue_tooSmall, 522),
        (stage_wrong, 523),
        (init_missing, 524),
        (memory_allocation, 525),
        (workSpace_tooSmall, 526),
        (dstSize_tooSmall, 527),
        (srcSize_wrong, 528),
        (dstBuffer_null, 529),
        (frameIndex_tooLarge, 530),
        (seekableIO, 531),
        (dstBuffer_wrong, 532),
        (srcBuffer_wrong, 533),
    }
}

c_xmacro! {
    BCH_ERRCODES(x) {
        (EIO, blockdev_io_error, 0),
        BLK_ERRS(BLK_STS(n, nr) => (BCH_ERR_blockdev_io_error, concat(BLK_STS_, n), nr)),
        (BCH_ERR_blockdev_io_error, BLK_STS_UNKNOWN, 20),
        (ERANGE, ERANGE_option_too_small, 21),
        (ERANGE, ERANGE_option_too_big, 22),
        (ERANGE, projid_too_big, 23),
        (EINVAL, injected, 24),
        (BCH_ERR_injected, injected_fs_start, 25),
        (EINVAL, mount_option, 26),
        (BCH_ERR_mount_option, option_name, 27),
        (BCH_ERR_mount_option, option_value, 28),
        (BCH_ERR_mount_option, option_not_bool, 29),
        (ENOMEM, ENOMEM_stripe_buf, 30),
        (ENOMEM, ENOMEM_replicas_table, 31),
        (ENOMEM, ENOMEM_cpu_replicas, 32),
        (ENOMEM, ENOMEM_replicas_gc, 33),
        (ENOMEM, ENOMEM_disk_groups_validate, 34),
        (ENOMEM, ENOMEM_disk_groups_to_cpu, 35),
        (ENOMEM, ENOMEM_mark_snapshot, 36),
        (ENOMEM, ENOMEM_mark_stripe, 37),
        (ENOMEM, ENOMEM_mark_stripe_ptr, 38),
        (ENOMEM, ENOMEM_btree_key_cache_create, 39),
        (ENOMEM, ENOMEM_btree_key_cache_fill, 40),
        (ENOMEM, ENOMEM_btree_key_cache_insert, 41),
        (ENOMEM, ENOMEM_trans_kmalloc, 42),
        (ENOMEM, ENOMEM_trans_log_msg, 43),
        (ENOMEM, ENOMEM_do_encrypt, 44),
        (ENOMEM, ENOMEM_ec_read_extent, 45),
        (ENOMEM, ENOMEM_ec_stripe_mem_alloc, 46),
        (ENOMEM, ENOMEM_ec_new_stripe_alloc, 47),
        (ENOMEM, ENOMEM_fs_btree_cache_init, 48),
        (ENOMEM, ENOMEM_fs_btree_key_cache_init, 49),
        (ENOMEM, ENOMEM_fs_counters_init, 50),
        (ENOMEM, ENOMEM_fs_btree_write_buffer_init, 51),
        (ENOMEM, ENOMEM_blacklist_table_init, 53),
        (ENOMEM, ENOMEM_sb_realloc_injected, 54),
        (ENOMEM, ENOMEM_sb_bio_realloc, 55),
        (ENOMEM, ENOMEM_sb_buf_realloc, 56),
        (ENOMEM, ENOMEM_sb_journal_validate, 57),
        (ENOMEM, ENOMEM_sb_journal_v2_validate, 58),
        (ENOMEM, ENOMEM_journal_entry_add, 59),
        (ENOMEM, ENOMEM_journal_read_buf_realloc, 60),
        (ENOMEM, ENOMEM_btree_interior_update_worker_init, 61),
        (ENOMEM, ENOMEM_btree_node_rewrites_table_init, 62),
        (ENOMEM, ENOMEM_btree_interior_update_pool_init, 63),
        (ENOMEM, ENOMEM_bio_read_init, 64),
        (ENOMEM, ENOMEM_bio_read_split_init, 65),
        (ENOMEM, ENOMEM_bio_write_init, 66),
        (ENOMEM, ENOMEM_promote_limit_init, 67),
        (ENOMEM, ENOMEM_bio_bounce_pages_init, 68),
        (ENOMEM, ENOMEM_writepage_bioset_init, 69),
        (ENOMEM, ENOMEM_writepage_buf_pool_init, 70),
        (ENOMEM, ENOMEM_dio_read_bioset_init, 71),
        (ENOMEM, ENOMEM_dio_write_bioset_init, 72),
        (ENOMEM, ENOMEM_nocow_flush_bioset_init, 73),
        (ENOMEM, ENOMEM_promote_table_init, 74),
        (ENOMEM, ENOMEM_async_obj_init, 75),
        (ENOMEM, ENOMEM_compression_bounce_read_init, 76),
        (ENOMEM, ENOMEM_compression_bounce_write_init, 77),
        (ENOMEM, ENOMEM_compression_workspace_init, 78),
        (ENOMEM, ENOMEM_backpointer_mismatches_bitmap, 79),
        (EIO, compression_workspace_not_initialized, 80),
        (ENOMEM, ENOMEM_bucket_gens, 81),
        (ENOMEM, ENOMEM_buckets_nouse, 82),
        (ENOMEM, ENOMEM_usage_init, 83),
        (ENOMEM, ENOMEM_btree_node_read_all_replicas, 84),
        (ENOMEM, ENOMEM_btree_node_reclaim, 85),
        (ENOMEM, ENOMEM_btree_node_mem_alloc, 86),
        (ENOMEM, ENOMEM_btree_cache_cannibalize_lock, 87),
        (ENOMEM, ENOMEM_set_nr_journal_buckets, 88),
        (ENOMEM, ENOMEM_dev_journal_init, 89),
        (ENOMEM, ENOMEM_journal_pin_fifo, 90),
        (ENOMEM, ENOMEM_journal_buf, 91),
        (ENOMEM, ENOMEM_gc_start, 92),
        (ENOMEM, ENOMEM_gc_alloc_start, 93),
        (ENOMEM, ENOMEM_gc_reflink_start, 94),
        (ENOMEM, ENOMEM_gc_gens, 95),
        (ENOMEM, ENOMEM_gc_repair_key, 96),
        (ENOMEM, ENOMEM_fsck_extent_ends_at, 97),
        (ENOMEM, ENOMEM_fsck_add_nlink, 98),
        (ENOMEM, ENOMEM_journal_key_insert, 99),
        (ENOMEM, ENOMEM_journal_keys_sort, 100),
        (ENOMEM, ENOMEM_read_superblock_clean, 101),
        (ENOMEM, ENOMEM_fs_alloc, 102),
        (ENOMEM, ENOMEM_fs_name_alloc, 103),
        (ENOMEM, ENOMEM_fs_other_alloc, 104),
        (ENOMEM, ENOMEM_dev_alloc, 105),
        (ENOMEM, ENOMEM_disk_accounting, 106),
        (ENOMEM, ENOMEM_stripe_head_alloc, 107),
        (ENOMEM, ENOMEM_journal_read_bucket, 108),
        (ENOMEM, ENOMEM_acl, 109),
        (ENOMEM, ENOMEM_move_extent, 110),
        (ENOMEM, ENOMEM_reconcile_scan_in_flight, 111),
        (ENOSPC, ENOSPC_disk_reservation, 112),
        (ENOSPC, ENOSPC_bucket_alloc, 113),
        (ENOSPC, ENOSPC_disk_label_add, 114),
        (ENOSPC, ENOSPC_stripe_create, 115),
        (ENOSPC, ENOSPC_inode_create, 116),
        (ENOSPC, ENOSPC_str_hash_create, 117),
        (ENOSPC, ENOSPC_snapshot_create, 118),
        (ENOSPC, ENOSPC_subvolume_create, 119),
        (ENOSPC, ENOSPC_sb, 120),
        (ENOSPC, ENOSPC_sb_journal, 121),
        (ENOSPC, ENOSPC_sb_journal_seq_blacklist, 122),
        (ENOSPC, ENOSPC_sb_quota, 123),
        (ENOSPC, ENOSPC_sb_replicas, 124),
        (ENOSPC, ENOSPC_sb_members, 125),
        (ENOSPC, ENOSPC_sb_members_v2, 126),
        (ENOSPC, ENOSPC_sb_extent_type_u64s, 127),
        (ENOSPC, ENOSPC_sb_crypt, 128),
        (ENOSPC, ENOSPC_sb_downgrade, 129),
        (ENOSPC, ENOSPC_btree_slot, 130),
        (ENOSPC, ENOSPC_snapshot_tree, 131),
        (ENOENT, ENOENT_bkey_type_mismatch, 132),
        (ENOENT, ENOENT_str_hash_lookup, 133),
        (ENOENT, ENOENT_str_hash_set_must_replace, 134),
        (ENOENT, ENOENT_inode, 135),
        (ENOENT, ENOENT_not_subvol, 136),
        (ENOENT, ENOENT_not_directory, 137),
        (ENOENT, ENOENT_directory_dead, 138),
        (ENOENT, ENOENT_subvolume, 139),
        (ENOENT, ENOENT_subvolume_deleted, 140),
        (ENOENT, ENOENT_snapshot, 141),
        (ENOENT, ENOENT_snapshot_tree, 142),
        (ENOENT, ENOENT_dirent_doesnt_match_inode, 143),
        (ENOENT, ENOENT_dev_not_found, 144),
        (ENOENT, ENOENT_dev_bucket_not_found, 145),
        (ENOENT, ENOENT_dev_idx_not_found, 146),
        (ENOENT, ENOENT_inode_no_backpointer, 147),
        (ENOENT, ENOENT_no_snapshot_tree_subvol, 148),
        (ENOENT, btree_node_dying, 149),
        (ENOTEMPTY, ENOTEMPTY_dir_not_empty, 150),
        (ENOTEMPTY, ENOTEMPTY_subvol_not_empty, 151),
        (EEXIST, EEXIST_str_hash_set, 152),
        (EEXIST, EEXIST_discard_in_flight_add, 153),
        (EEXIST, EEXIST_subvolume_create, 154),
        (EAGAIN, open_buckets_empty, 155),
        (EAGAIN, freelist_empty, 156),
        (EAGAIN, stripe_insufficient_devices, 158),
        (EAGAIN, max_discards_in_flight, 159),
        (ENOSPC, ec_alloc_failed, 160),
        (BCH_ERR_freelist_empty, no_buckets_found, 161),
        (BCH_ERR_freelist_empty, bucket_alloc_no_progress, 162),
        (0, transaction_restart, 163),
        (BCH_ERR_transaction_restart, transaction_restart_fault_inject, 164),
        (BCH_ERR_transaction_restart, transaction_restart_relock, 165),
        (BCH_ERR_transaction_restart, transaction_restart_relock_path, 166),
        (BCH_ERR_transaction_restart, transaction_restart_relock_path_intent, 167),
        (BCH_ERR_transaction_restart, transaction_restart_too_many_iters, 168),
        (BCH_ERR_transaction_restart, transaction_restart_lock_node_reused, 169),
        (BCH_ERR_transaction_restart, transaction_restart_fill_relock, 170),
        (BCH_ERR_transaction_restart, transaction_restart_fill_mem_alloc_fail, 171),
        (BCH_ERR_transaction_restart, transaction_restart_lock_waitlist_alloc, 172),
        (BCH_ERR_transaction_restart, transaction_restart_mem_realloced, 173),
        (BCH_ERR_transaction_restart, transaction_restart_in_traverse_all, 174),
        (BCH_ERR_transaction_restart, transaction_restart_would_deadlock, 175),
        (BCH_ERR_transaction_restart, transaction_restart_would_deadlock_write, 176),
        (BCH_ERR_transaction_restart, transaction_restart_deadlock_recursion_limit, 177),
        (BCH_ERR_transaction_restart, transaction_restart_deadlock_waitlist_alloc, 178),
        (BCH_ERR_transaction_restart, transaction_restart_upgrade, 179),
        (BCH_ERR_transaction_restart, transaction_restart_key_cache_fill, 180),
        (BCH_ERR_transaction_restart, transaction_restart_key_cache_raced, 181),
        (BCH_ERR_transaction_restart, transaction_restart_lock_root_race, 182),
        (BCH_ERR_transaction_restart, transaction_restart_split_race, 183),
        (BCH_ERR_transaction_restart, transaction_restart_split_with_interior_updates, 184),
        (BCH_ERR_transaction_restart, transaction_restart_write_buffer_flush, 185),
        (BCH_ERR_transaction_restart, transaction_restart_nested, 186),
        (BCH_ERR_transaction_restart, transaction_restart_commit, 187),
        (BCH_ERR_transaction_restart, transaction_restart_journal_overwrites_changed, 188),
        (0, no_btree_node, 189),
        (BCH_ERR_no_btree_node, no_btree_node_relock, 190),
        (BCH_ERR_no_btree_node, no_btree_node_upgrade, 191),
        (BCH_ERR_no_btree_node, no_btree_node_drop, 192),
        (BCH_ERR_no_btree_node, no_btree_node_lock_root, 193),
        (BCH_ERR_no_btree_node, no_btree_node_up, 194),
        (BCH_ERR_no_btree_node, no_btree_node_down, 195),
        (BCH_ERR_no_btree_node, no_btree_node_init, 196),
        (BCH_ERR_no_btree_node, no_btree_node_cached, 197),
        (BCH_ERR_no_btree_node, no_btree_node_srcu_reset, 198),
        (BCH_ERR_no_btree_node, no_btree_node_nofill, 199),
        (BCH_ERR_no_btree_node, no_btree_node_reused, 200),
        (BCH_ERR_no_btree_node, no_btree_node_stale_paths, 201),
        (0, btree_insert_fail, 202),
        (BCH_ERR_btree_insert_fail, btree_insert_btree_node_full, 203),
        (BCH_ERR_btree_insert_fail, btree_insert_need_mark_replicas, 204),
        (BCH_ERR_btree_insert_fail, btree_insert_need_journal_res, 205),
        (BCH_ERR_btree_insert_fail, btree_insert_need_journal_reclaim, 206),
        (0, backpointer_to_overwritten_btree_node, 207),
        (0, journal_reclaim_would_deadlock, 208),
        (0, journal_rewind_no_overwrites, 209),
        (EROFS, fsck, 210),
        (BCH_ERR_fsck, fsck_ask, 211),
        (BCH_ERR_fsck, fsck_fix, 212),
        (BCH_ERR_fsck, fsck_delete_bkey, 213),
        (BCH_ERR_fsck, fsck_ignore, 214),
        (BCH_ERR_fsck, fsck_errors_not_fixed, 215),
        (BCH_ERR_fsck, fsck_repair_unimplemented, 216),
        (BCH_ERR_fsck, fsck_repair_impossible, 217),
        (EINVAL, recovery_will_run, 218),
        (BCH_ERR_recovery_will_run, restart_recovery, 219),
        (BCH_ERR_recovery_will_run, cannot_rewind_recovery, 220),
        (BCH_ERR_recovery_will_run, recovery_pass_will_run, 221),
        (0, bkey_was_deleted, 222),
        (0, bucket_not_moveable, 223),
        (BCH_ERR_bucket_not_moveable, bucket_not_moveable_dev_not_rw, 224),
        (BCH_ERR_bucket_not_moveable, bucket_not_moveable_bucket_open, 225),
        (BCH_ERR_bucket_not_moveable, bucket_not_moveable_bp_mismatch, 226),
        (BCH_ERR_bucket_not_moveable, bucket_not_moveable_lru_race, 227),
        (0, data_update_done, 228),
        (BCH_ERR_data_update_done, data_update_done_unwritten, 229),
        (BCH_ERR_data_update_done, data_update_done_no_writes_needed, 230),
        (0, data_update_fail, 231),
        (BCH_ERR_data_update_fail, data_update_fail_would_block, 232),
        (BCH_ERR_data_update_fail, data_update_fail_in_flight, 233),
        (BCH_ERR_data_update_fail, data_update_fail_no_snapshot, 234),
        (BCH_ERR_data_update_fail, data_update_fail_no_rw_devs, 235),
        (BCH_ERR_data_update_fail, data_update_fail_need_copygc, 236),
        (EPERM, reflink_p_may_update_options_unset, 237),
        (EPERM, EPERM_non_admin, 238),
        (EPERM, EPERM_non_admin_or_owner, 239),
        (EINVAL, device_state_not_allowed, 240),
        (EINVAL, member_info_missing, 241),
        (EINVAL, mismatched_block_size, 242),
        (EINVAL, block_size_too_small, 243),
        (EINVAL, bucket_size_too_small, 244),
        (EINVAL, device_size_too_small, 245),
        (EINVAL, device_size_too_big, 246),
        (EINVAL, device_not_a_member_of_filesystem, 247),
        (EINVAL, device_has_been_removed, 248),
        (EINVAL, device_splitbrain, 249),
        (EINVAL, device_already_online, 250),
        (EINVAL, filesystem_uuid_already_open, 251),
        (EINVAL, insufficient_devices_to_start, 252),
        (EINVAL, chardev_init_error, 253),
        (EINVAL, sysfs_init_error, 254),
        (EINVAL, invalid, 255),
        (EINVAL, internal_fsck_err, 256),
        (EINVAL, opt_parse_error, 257),
        (EINVAL, remove_with_metadata_missing_unimplemented, 258),
        (EINVAL, remove_would_lose_data, 259),
        (EINVAL, remove_by_backpointer_did_not_terminate, 260),
        (EINVAL, remove_stripes_did_not_terminate, 261),
        (EINVAL, no_resize_with_buckets_nouse, 262),
        (EINVAL, inode_unpack_error, 263),
        (EINVAL, inode_not_unlinked, 264),
        (EINVAL, inode_has_child_snapshot, 265),
        (EINVAL, inode_is_subvolume_root, 266),
        (EINVAL, varint_decode_error, 267),
        (EINVAL, erasure_coding_found_btree_node, 268),
        (EINVAL, erasure_coding_stripe_update_err, 269),
        (EINVAL, stripe_unknown_csum_type, 270),
        (EINVAL, option_negative, 271),
        (EINVAL, topology_repair, 272),
        (EINVAL, EINVAL_unaligned_io, 273),
        (EINVAL, EINVAL_rename_bad_flags, 274),
        (EINVAL, EINVAL_setattr_bad_file_type, 275),
        (EINVAL, EINVAL_get_name_not_dir, 276),
        (EINVAL, EINVAL_reconfigure_read_write, 277),
        (EINVAL, EINVAL_fcollapse_finsert_unaligned, 278),
        (EINVAL, EINVAL_finsert_past_eof, 279),
        (EINVAL, EINVAL_fcollapse_past_eof, 280),
        (EINVAL, EINVAL_remap_bad_flags, 281),
        (EINVAL, EINVAL_remap_unaligned, 282),
        (EINVAL, EINVAL_remap_overlapping, 283),
        (EINVAL, EINVAL_setlabel_too_long, 284),
        (EINVAL, EINVAL_goingdown_bad_flags, 285),
        (EINVAL, EINVAL_subvol_create_bad_flags, 286),
        (EINVAL, EINVAL_subvol_create_flags_mismatch, 287),
        (EINVAL, EINVAL_subvol_destroy_bad_flags, 288),
        (EINVAL, EINVAL_subvol_readdir_pad, 289),
        (EINVAL, EINVAL_subvol_to_path_no_buf, 290),
        (EINVAL, EINVAL_snapshot_tree_query_pad, 291),
        (EINVAL, EINVAL_fiemap_overflow, 292),
        (EINVAL, EINVAL_snapshot_not_subvol_root, 293),
        (EINVAL, EINVAL_quota_enable_acct, 294),
        (EINVAL, EINVAL_quota_enable_usrquota, 295),
        (EINVAL, EINVAL_quota_enable_grpquota, 296),
        (EINVAL, EINVAL_quota_enable_prjquota, 297),
        (EINVAL, EINVAL_quota_remove_usrquota, 298),
        (EINVAL, EINVAL_quota_remove_grpquota, 299),
        (EINVAL, EINVAL_quota_remove_prjquota, 300),
        (EINVAL, EINVAL_quota_set_info_bad_type, 301),
        (EINVAL, EINVAL_quota_set_info_bad_field, 302),
        (EINVAL, EINVAL_xattr_get_bad_opt, 303),
        (EINVAL, EINVAL_xattr_get_not_inode_opt, 304),
        (EINVAL, EINVAL_xattr_set_bad_opt, 305),
        (EINVAL, EINVAL_xattr_set_not_inode_opt, 306),
        (EINVAL, EINVAL_fsck_offline_bad_flags, 307),
        (EINVAL, EINVAL_fsck_online_bad_passes, 308),
        (EINVAL, EINVAL_fsck_online_bad_flags, 309),
        (EINVAL, EINVAL_journal_replay_key_bad_btree_depth, 310),
        (EINVAL, EINVAL_dev_resize_shrink, 311),
        (EINVAL, EINVAL_missing_new_extent_overwrite, 312),
        (EINVAL, EINVAL_version_min_too_old, 313),
        (EINVAL, EINVAL_block_size_needs_thp, 314),
        (EINVAL, EINVAL_utf8_load_failed, 315),
        (EINVAL, EINVAL_casefolding_no_unicode, 316),
        (EINVAL, EINVAL_no_version_check_start, 317),
        (EINVAL, EINVAL_ioctl_disk_add_bad_flags, 318),
        (EINVAL, EINVAL_ioctl_disk_add_v2_bad_flags, 319),
        (EINVAL, EINVAL_ioctl_disk_remove_bad_flags, 320),
        (EINVAL, EINVAL_ioctl_disk_remove_v2_bad_flags, 321),
        (EINVAL, EINVAL_ioctl_disk_online_bad_flags, 322),
        (EINVAL, EINVAL_ioctl_disk_online_v2_bad_flags, 323),
        (EINVAL, EINVAL_ioctl_disk_offline_bad_flags, 324),
        (EINVAL, EINVAL_ioctl_disk_offline_v2_bad_flags, 325),
        (EINVAL, EINVAL_ioctl_disk_set_state_bad_args, 326),
        (EINVAL, EINVAL_ioctl_disk_set_state_v2_bad_args, 327),
        (EINVAL, EINVAL_ioctl_data_read_short_buf, 328),
        (EINVAL, EINVAL_ioctl_data_bad_op, 329),
        (EINVAL, EINVAL_ioctl_fs_usage_accounting_not_read, 330),
        (EINVAL, EINVAL_ioctl_query_accounting_not_read, 331),
        (EINVAL, EINVAL_ioctl_dev_usage_accounting_not_read, 332),
        (EINVAL, EINVAL_ioctl_dev_usage_bad_flags, 333),
        (EINVAL, EINVAL_ioctl_dev_usage_v2_accounting_not_read, 334),
        (EINVAL, EINVAL_ioctl_dev_usage_v2_bad_flags, 335),
        (EINVAL, EINVAL_ioctl_query_btree_keys_bad_flags, 336),
        (EINVAL, EINVAL_ioctl_query_btree_keys_bad_params, 337),
        (EINVAL, EINVAL_ioctl_read_super_bad_flags, 338),
        (EINVAL, EINVAL_ioctl_disk_get_idx_bad_dev, 339),
        (EINVAL, EINVAL_ioctl_disk_resize_bad_flags, 340),
        (EINVAL, EINVAL_ioctl_disk_resize_v2_bad_flags, 341),
        (EINVAL, EINVAL_ioctl_disk_resize_journal_bad_flags, 342),
        (EINVAL, EINVAL_ioctl_disk_resize_journal_too_big, 343),
        (EINVAL, EINVAL_ioctl_disk_resize_journal_v2_bad_flags, 344),
        (EINVAL, EINVAL_ioctl_disk_resize_journal_v2_too_big, 345),
        (EINVAL, EINVAL_ioctl_not_started, 346),
        (EINVAL, EINVAL_journal_write_overran_available_space, 347),
        (EINVAL, EINVAL_journal_bucket_not_found, 348),
        (EINVAL, EINVAL_journal_seq_overflow, 349),
        (EINVAL, EINVAL_journal_entry_version_incompatible, 350),
        (EINVAL, EINVAL_journal_validate_version_incompatible, 351),
        (EINVAL, EINVAL_journal_rewind_before_discard, 352),
        (EINVAL, EINVAL_opt_target_parse_not_found, 353),
        (EINVAL, EINVAL_disable_encryption_no_crypt, 354),
        (EINVAL, EINVAL_reflink_gc_table_mismatch, 355),
        (EINVAL, EINVAL_ec_stripe_create_existing_key, 356),
        (EINVAL, EINVAL_finsert_offset_past_eof, 357),
        (EINVAL, EINVAL_snapshot_parent_already_has_children, 359),
        (EINVAL, EINVAL_snapshot_delete_has_two_children, 360),
        (EINVAL, EINVAL_snapshot_delete_interior_at_runtime, 361),
        (EINVAL, EINVAL_snapshot_delete_with_data, 362),
        (EINVAL, EINVAL_snapshot_delete_already_deleted, 363),
        (EINVAL, EINVAL_snapshot_delete_bad_subvol, 364),
        (EINVAL, EINVAL_snapshot_delete_bad_topology, 365),
        (EINVAL, EINVAL_snapshot_parent_missing_child_ptr, 366),
        (EINVAL, EINVAL_snapshot_child_bad_parent, 367),
        (EINVAL, EINVAL_snapshot_edge_to_missing_node, 368),
        (EINVAL, EINVAL_snapshot_bad_subvol_flag, 369),
        (EINVAL, EINVAL_opt_parse_uint_required, 370),
        (EINVAL, EINVAL_opt_parse_str_required, 371),
        (EINVAL, EINVAL_test_zero_nr_or_threads, 372),
        (EINVAL, EINVAL_test_unknown_test, 373),
        (EINVAL, EINVAL_sysfs_opt_not_found, 374),
        (EINVAL, EINVAL_ioctl_query_counters_bad_flags, 375),
        (EINVAL, EINVAL_node_scan_no_nodes, 376),
        (EINVAL, EINVAL_node_scan_too_many_replicas, 377),
        (EINVAL, EINVAL_parse_btree_id, 378),
        (EINVAL, EINVAL_parse_bkey_type, 379),
        (EINVAL, EINVAL_parse_bpos, 380),
        (EINVAL, EINVAL_parse_bbpos, 381),
        (BCH_ERR_topology_repair, topology_repair_drop_this_node, 382),
        (BCH_ERR_topology_repair, topology_repair_drop_prev_node, 383),
        (BCH_ERR_topology_repair, topology_repair_did_fill_from_scan, 384),
        (EMLINK, too_many_links, 385),
        (EOPNOTSUPP, may_not_use_incompat_feature, 386),
        (EOPNOTSUPP, no_casefolding_without_utf8, 387),
        (EOPNOTSUPP, casefolding_disabled, 388),
        (EOPNOTSUPP, casefold_opt_is_dir_only, 389),
        (EOPNOTSUPP, casefolding_in_use, 390),
        (EOPNOTSUPP, casefold_dir_but_disabled, 391),
        (EOPNOTSUPP, unsupported_fsx_flag, 392),
        (EOPNOTSUPP, unsupported_fa_flag, 393),
        (EOPNOTSUPP, unsupported_fallocate_mode, 394),
        (EROFS, erofs_trans_commit, 395),
        (EROFS, erofs_no_writes, 396),
        (EROFS, erofs_journal_err, 397),
        (EROFS, erofs_sb_err, 398),
        (EROFS, erofs_unfixed_errors, 399),
        (EROFS, erofs_norecovery, 400),
        (EROFS, erofs_nochanges, 401),
        (EROFS, erofs_no_alloc_info, 402),
        (EROFS, erofs_filesystem_full, 403),
        (EROFS, erofs_sb_not_migrated, 404),
        (EROFS, insufficient_devices, 405),
        (EROFS, erofs_recovery_cancelled, 406),
        (EROFS, emergency_ro, 407),
        (ESHUTDOWN, btree_not_started, 408),
        (0, operation_blocked, 409),
        (BCH_ERR_operation_blocked, btree_cache_cannibalize_lock_blocked, 410),
        (BCH_ERR_operation_blocked, journal_res_blocked, 411),
        (BCH_ERR_operation_blocked, bucket_alloc_blocked, 412),
        (BCH_ERR_operation_blocked, open_bucket_alloc_blocked, 413),
        (BCH_ERR_operation_blocked, stripe_alloc_blocked, 414),
        (BCH_ERR_operation_blocked, stripe_buf_mem_blocked, 415),
        (EAGAIN, stripe_buf_mem_limit, 416),
        (BCH_ERR_journal_res_blocked, journal_blocked, 417),
        (BCH_ERR_journal_res_blocked, journal_max_in_flight, 418),
        (BCH_ERR_journal_res_blocked, journal_max_open, 419),
        (BCH_ERR_journal_res_blocked, journal_full, 420),
        (BCH_ERR_journal_res_blocked, journal_pin_full, 421),
        (BCH_ERR_journal_res_blocked, journal_buf_enomem, 422),
        (BCH_ERR_journal_res_blocked, journal_stuck, 423),
        (BCH_ERR_journal_res_blocked, journal_retry_open, 424),
        (BCH_ERR_invalid, invalid_sb, 425),
        (BCH_ERR_invalid_sb, invalid_sb_magic, 426),
        (BCH_ERR_invalid_sb, invalid_sb_version, 427),
        (BCH_ERR_invalid_sb, invalid_sb_features, 428),
        (BCH_ERR_invalid_sb, invalid_sb_too_big, 429),
        (BCH_ERR_invalid_sb, invalid_sb_csum_type, 430),
        (BCH_ERR_invalid_sb, invalid_sb_csum, 431),
        (BCH_ERR_invalid_sb, invalid_sb_block_size, 432),
        (BCH_ERR_invalid_sb, invalid_sb_uuid, 433),
        (BCH_ERR_invalid_sb, invalid_sb_offset, 434),
        (BCH_ERR_invalid_sb, invalid_sb_too_many_members, 435),
        (BCH_ERR_invalid_sb, invalid_sb_dev_idx, 436),
        (BCH_ERR_invalid_sb, invalid_sb_time_precision, 437),
        (BCH_ERR_invalid_sb, invalid_sb_field_size, 438),
        (BCH_ERR_invalid_sb, invalid_sb_layout, 439),
        (BCH_ERR_invalid_sb_layout, invalid_sb_layout_type, 440),
        (BCH_ERR_invalid_sb_layout, invalid_sb_layout_nr_superblocks, 441),
        (BCH_ERR_invalid_sb_layout, invalid_sb_layout_superblocks_overlap, 442),
        (BCH_ERR_invalid_sb_layout, invalid_sb_layout_sb_max_size_bits, 443),
        (BCH_ERR_invalid_sb, invalid_sb_members_missing, 444),
        (BCH_ERR_invalid_sb, invalid_sb_members, 445),
        (BCH_ERR_invalid_sb, invalid_sb_disk_groups, 446),
        (BCH_ERR_invalid_sb, invalid_sb_replicas, 447),
        (BCH_ERR_invalid_sb, invalid_replicas_entry, 448),
        (BCH_ERR_invalid_sb, invalid_sb_journal, 449),
        (BCH_ERR_invalid_sb, invalid_sb_journal_seq_blacklist, 450),
        (BCH_ERR_invalid_sb, invalid_sb_crypt, 451),
        (BCH_ERR_invalid_sb, invalid_sb_clean, 452),
        (BCH_ERR_invalid_sb, invalid_sb_quota, 453),
        (BCH_ERR_invalid_sb, invalid_sb_errors, 454),
        (BCH_ERR_invalid_sb, invalid_sb_opt_compression, 455),
        (BCH_ERR_invalid_sb, invalid_sb_ext, 456),
        (BCH_ERR_invalid_sb, invalid_sb_downgrade, 457),
        (BCH_ERR_invalid_sb, invalid_sb_extent_type_u64s, 458),
        (BCH_ERR_invalid, invalid_bkey, 459),
        (BCH_ERR_operation_blocked, nocow_lock_blocked, 460),
        (EROFS, journal_shutdown, 461),
        (EIO, journal_flush_err, 462),
        (EIO, journal_write_err, 463),
        (EIO, btree_node_read_err, 464),
        (EIO, btree_node_validate_err, 465),
        (BCH_ERR_btree_node_read_err, btree_node_read_err_cached, 466),
        (EIO, sb_not_downgraded, 467),
        (EIO, btree_node_write_all_failed, 468),
        (EIO, btree_node_read_error, 469),
        (EIO, btree_root_error_unset, 470),
        (EIO, btree_need_topology_repair, 471),
        (EIO, bucket_ref_update, 472),
        (EIO, trigger_alloc, 473),
        (EIO, trigger_pointer, 474),
        (EIO, trigger_stripe_pointer, 475),
        (EIO, metadata_bucket_inconsistency, 476),
        (EIO, mark_stripe, 477),
        (EIO, stripe_read, 478),
        (BCH_ERR_stripe_read, stripe_read_device_offline, 479),
        (BCH_ERR_stripe_read, stripe_read_ptr_stale, 480),
        (BCH_ERR_stripe_read, stripe_read_csum_err, 481),
        (BCH_ERR_stripe_read, stripe_reconstruct, 482),
        (BCH_ERR_stripe_read, stripe_reconstruct_enomem, 483),
        (BCH_ERR_stripe_read, stripe_reconstruct_insufficient_blocks, 484),
        (BCH_ERR_stripe_read, stripe_reconstruct_stale_race, 485),
        (EIO, key_type_error, 486),
        (EIO, extent_poisoned, 487),
        (EIO, missing_indirect_extent, 488),
        (EIO, invalidate_stripe_to_dev, 489),
        (EIO, no_encryption_key, 490),
        (EIO, insufficient_journal_devices, 491),
        (EIO, device_offline, 492),
        (EIO, stripe_create_device_offline, 493),
        (EROFS, stripe_create_device_removing, 494),
        (EIO, EIO_fault_injected, 495),
        (EIO, ec_block_read, 496),
        (EIO, ec_block_write, 497),
        (EIO, recompute_checksum, 498),
        (BCH_ERR_data_read_retry_avoid,decompress, 499),
        (BCH_ERR_decompress, decompress_exceeded_max_encoded_extent, 500),
        (BCH_ERR_decompress, decompress_lz4_old, 501),
        (BCH_ERR_decompress, decompress_lz4, 502),
        (BCH_ERR_decompress, decompress_gzip, 503),
        (BCH_ERR_decompress, decompress_gzip_size_mismatch, 504),
        (BCH_ERR_decompress, decompress_zstd_src_len_bad, 505),
        (BCH_ERR_decompress, decompress_zstd_size_mismatch, 506),
        (BCH_ERR_decompress, zstd_error, 507),
        ZSTD_ERRS(ZSTD_error(n, nr) => (BCH_ERR_zstd_error, concat(ZSTD_error_, n), nr)),
        (BCH_ERR_zstd_error, ZSTD_error_unknown, 534),
        (EIO, data_write, 535),
        (BCH_ERR_data_write, data_write_io, 536),
        (BCH_ERR_data_write, data_write_csum, 537),
        (BCH_ERR_data_write, data_write_invalid_ptr, 538),
        (BCH_ERR_data_write, data_write_misaligned, 539),
        (BCH_ERR_data_write, data_write_need_fresh_buckets, 540),
        (EIO, data_read, 541),
        (BCH_ERR_data_read, no_device_to_read_from, 542),
        (BCH_ERR_data_read, no_devices_valid, 543),
        (BCH_ERR_data_read, data_read_io_err, 544),
        (BCH_ERR_data_read, data_read_csum_err, 545),
        (BCH_ERR_data_read, data_read_retry, 546),
        (BCH_ERR_data_read_retry, data_read_retry_avoid, 547),
        (BCH_ERR_data_read_retry_avoid,data_read_retry_device_offline, 548),
        (BCH_ERR_data_read_retry_avoid,data_read_retry_io_err, 549),
        (BCH_ERR_data_read_retry_avoid,data_read_retry_ec_reconstruct_err, 550),
        (BCH_ERR_data_read_retry_avoid,data_read_retry_csum_err, 551),
        (BCH_ERR_data_read_retry, data_read_retry_csum_err_maybe_userspace, 552),
        (BCH_ERR_data_read_retry_avoid,data_read_decompress_err, 553),
        (BCH_ERR_data_read_retry_avoid,data_read_decrypt_err, 554),
        (BCH_ERR_data_read, data_read_ptr_stale_race, 555),
        (BCH_ERR_data_read_retry, data_read_ptr_stale_retry, 556),
        (BCH_ERR_data_read_retry_avoid,data_read_ptr_stale_dirty, 557),
        (BCH_ERR_data_read, data_read_no_encryption_key, 558),
        (BCH_ERR_data_read, data_read_buffer_too_small, 559),
        (BCH_ERR_data_read, data_read_key_overwritten, 560),
        (0, rbio_narrow_crcs_fail, 561),
        (0, nopromote, 562),
        (BCH_ERR_nopromote, nopromote_no_rewrites, 563),
        (BCH_ERR_nopromote, nopromote_already_promoted, 564),
        (BCH_ERR_nopromote, nopromote_unwritten, 565),
        (BCH_ERR_nopromote, nopromote_congested, 566),
        (BCH_ERR_nopromote, nopromote_ratelimited, 567),
        (BCH_ERR_nopromote, nopromote_no_writes, 568),
        (BCH_ERR_nopromote, nopromote_enomem, 569),
        (0, snapshot, 570),
        (BCH_ERR_snapshot, invalid_snapshot_node, 571),
        (BCH_ERR_snapshot, snapshot_multiple_descendents, 572),
        (BCH_ERR_snapshot, snapshot_lostfound_unreachable, 573),
        (0, option_needs_open_fs, 574),
        (0, remove_disk_accounting_entry, 575),
        (0, nocow_trylock_fail, 576),
        (BCH_ERR_nocow_trylock_fail, nocow_trylock_contended, 577),
        (BCH_ERR_nocow_trylock_fail, nocow_trylock_bucket_full, 578),
        (EINTR, cancelled, 579),
        (BCH_ERR_cancelled, recovery_cancelled, 580),
        (BCH_ERR_cancelled, kthread_cancelled, 581),
        (BCH_ERR_cancelled, snapshot_delete_cancelled, 582),
        (0, shutdown_with_errors, 583),
        (BCH_ERR_shutdown_with_errors, shutdown_with_errors_fixed, 584),
        (BCH_ERR_shutdown_with_errors, shutdown_with_errors_unfixed, 585),
        (BCH_ERR_shutdown_with_errors, shutdown_with_emergency_ro, 586),
        (BCH_ERR_insufficient_devices_to_start, insufficient_devices_data_intact, 587),
        (BCH_ERR_insufficient_devices_to_start, insufficient_devices_data_lost, 588),
        (EIO, injected_logged_op_fail, 589),
        (EINVAL, EINVAL_crypt_no_kdf_params, 590),
        (EIO, crypt_kdf_failed, 591),
        (0, reconcile_scan_stop, 592),
        (0, str_hash_key_repaired, 593),
        (0, delete_range_done, 594),
        (0, write_encoded_as_is, 596),
        (0, fc_continue, 600),
        (0, fc_break, 601),
        (0, cached_ptr_stale, 602),
        (BCH_ERR_no_btree_node, no_btree_node_shallow, 603),
        (0, bucket_not_allocatable, 604),
        (ENOSPC, ec_alloc_failed_pending_race, 605),
        (BCH_ERR_ec_alloc_failed, ec_alloc_failed_no_redundancy, 606),
        (BCH_ERR_ec_alloc_failed, ec_alloc_failed_insufficient_devs, 607),
        (BCH_ERR_ec_alloc_failed, ec_alloc_failed_disk_label_too_big, 608),
        (BCH_ERR_ec_alloc_failed, ec_alloc_failed_transient, 612),
        (BCH_ERR_ec_alloc_failed_transient, ec_alloc_failed_stripe_alloc, 609),
        (BCH_ERR_ec_alloc_failed_transient, ec_alloc_failed_no_usable_block, 610),
        (BCH_ERR_ec_alloc_failed_transient, ec_alloc_failed_no_ec_bucket, 611),
        (EIO, mmap_emergency_ro, 613),
        (EINVAL, EINVAL_test_stripe_already_open, 614),
        (EINVAL, EINVAL_test_stripe_open_relock_not_invalidated, 615),
        (EAGAIN, test_no_quiet_window, 616),
        (ENOMEM, ENOMEM_perf_test_job, 617),
        (EROFS, erofs_sb_write_before_start, 619),
        (EINVAL, EINVAL_will_not_start, 620),
        (EROFS, erofs_sb_never_started, 621),
        (ESHUTDOWN, ioctl_fs_stopping, 622),
        (ENOMEM, ENOMEM_reconcile_stripes_pending, 623),
        (BCH_ERR_btree_node_validate_err, btree_node_validate_err_keys_lost, 624),
        (ENOMEM, ENOMEM_backpointer_scan, 626),
        (BCH_ERR_transaction_restart, transaction_restart_backpointer_scan_grow, 627),
        (ENOMEM, ENOMEM_stripe_open, 628),
        (ENOMEM, ENOMEM_stripe_new_bucket, 629),
        (BCH_ERR_btree_node_validate_err, btree_node_validate_err_csum, 630),
        (EBUSY, EBUSY_journal_bucket_retire_rw, 631),
        (EBUSY, EBUSY_journal_bucket_retire_no_space, 632),
    }
}

/*
 * BCH_ERR_MAX is one past the highest errcode number, wherever that entry sits
 * in BCH_ERRCODES(): a union is the size of its largest member, and a char[nr
 * + 1] has no padding, so this one is the largest nr, plus one. Never declared
 * as an object, only measured.
 */
macro_rules! __bch_errcode_max_0 {
    ([$($acc:tt)*] $(($class:tt, $err:tt, $nr:tt)),* $(,)?) => { ::paste::paste! {
        #[allow(non_snake_case)]
        #[repr(C)]
        #[derive(Clone, Copy, CStruct, TypeInfo)]
        pub union bch_errcode_max {
            $($acc)*
            $(pub [<BCH_ERR_ $err>]: [crate::util::ffi::c_char; (($nr) + 1) as usize],)*
        }
    } };
}
BCH_ERRCODES!(__bch_errcode_max_0 []);
c_default!(bch_errcode_max);

c_enum! {
    #[open]
    pub enum bch_errcode: u32 {
        BCH_ERR_START = 2048,
        BCH_ERRCODES(class, err, nr) => concat(BCH_ERR_, err) = (c::BCH_ERR_START as u32) + (nr as u32),
        BCH_ERR_MAX = (c::BCH_ERR_START as u32)
        + (core::mem::size_of::<c::bch_errcode_max>() as u32),
    }
}

c_extern! {
    pub fn __bch2_err_class(bch_err: c_int) -> c_int;
}

// The status's, beside the errcode of the same name
pub mod status {
    use cstruct_macros::c_const;

    c_const! {
        /* we're getting away from reusing bi_status, this should go away */
        pub const BLK_STS_REMOVED: u8 = 128;
    }
}

// blk_status_t: a u8, which errcode_gen.h can name before blk_types.h has
#[c_export]
pub fn blk_status_to_bch_err(err: u8) -> c::bch_errcode {
    macro_rules! statuses {
        ($(($n:ident, $nr:tt)),* $(,)?) => { ::paste::paste! {
            match err {
                0 => bch_errcode(0),
                $(c::[<BLK_STS_ $n>] => bch_errcode::[<BCH_ERR_BLK_STS_ $n>],)*
                _ => bch_errcode::BCH_ERR_BLK_STS_UNKNOWN,
            }
        } };
    }
    c::BLK_ERRS!(statuses)
}

// Each errcode by its BCH_ERRCODES() name: transaction_restart for
// bch_errcode::BCH_ERR_transaction_restart.
macro_rules! errcode_names {
    ($(($class:tt, $err:tt, $nr:tt)),* $(,)?) => { ::paste::paste! { $(
        #[allow(non_upper_case_globals)]
        pub const $err: bch_errcode = bch_errcode::[<BCH_ERR_ $err>];
    )* } };
}
c::BCH_ERRCODES!(errcode_names);

const START: u32 = bch_errcode::BCH_ERR_START.0;
const N: usize = (bch_errcode::BCH_ERR_MAX.0 - START) as usize;

#[allow(dead_code, non_upper_case_globals)]
mod class {
    #[cfg(kernel)]
    pub(super) use crate::c::*;
    #[cfg(not(kernel))]
    pub(super) use linux_raw_sys::errno::*;

    macro_rules! codes {
        ($(($class:tt, $err:tt, $nr:tt)),* $(,)?) => { ::paste::paste! { $(
            pub(super) const [<BCH_ERR_ $err>]: u32 = super::bch_errcode::[<BCH_ERR_ $err>].0;
        )* } };
    }
    crate::c::BCH_ERRCODES!(codes);
}

macro_rules! class_of {
    (0) => { 0 };
    ($class:ident) => { class::$class as u32 };
}

macro_rules! errcode_table {
    ($(($class:tt, $err:tt, $nr:tt)),* $(,)?) => {
        static ERRCODES: [Option<(&str, u32)>; N] = {
            let mut t = [None; N];
            $(t[$nr] = Some((stringify!($err), class_of!($class)));)*
            t
        };
    };
}
c::BCH_ERRCODES!(errcode_table);

fn errcode(err: u32) -> Option<(&'static str, u32)> {
    ERRCODES.get(err.checked_sub(START)? as usize).copied().flatten()
}

fn errname(err: u32) -> Option<&'static str> {
    #[cfg(kernel)]
    let p = unsafe { c::errname(err as c_int) };
    #[cfg(not(kernel))]
    let p = {
        unsafe extern "C" { fn strerror(err: c_int) -> *const core::ffi::c_char; }
        unsafe { strerror(err as c_int) }
    };
    // SAFETY: a static string, or NULL
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p.cast()) }).and_then(|s| s.to_str().ok())
}

/// A bcachefs error code or errno, positive: errnos below BCH_ERR_START (2048),
/// bcachefs's own above. Unlike bch_errcode, a raw errno never makes an invalid
/// discriminant.
///
/// Positive always: C's -errcode is negative only at the boundary. Taken in,
/// either sign is the same error, so ==, raw() and ret_to_c() can't see C's
/// sign - a negative one kept would go back to C as success.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct BchError(i32);

// The bcachefs codes extend the errnos, so every errno is a BchError - as the
// bindings spell them (u32) or as libc does (i32), positive either way.

impl From<bch_errcode> for BchError {
    fn from(code: bch_errcode) -> Self { Self(code.0 as i32) }
}

impl From<u32> for BchError {
    fn from(errno: u32) -> Self { Self(errno as i32) }
}

impl From<i32> for BchError {
    fn from(errno: i32) -> Self { Self::from_raw(errno) }
}

impl BchError {
    /// An error code of either sign: C's -errcode, or an errno.
    pub fn from_raw(code: i32) -> Self { Self(code.unsigned_abs() as i32) }

    pub fn raw(&self) -> i32 { self.0 }

    /// The plain errno this is a case of, negated: for the VFS or userspace.
    pub fn class(&self) -> c_int {
        -self.errno()
    }

    pub fn msg(&self) -> &'static str {
        let name = match self.0.unsigned_abs() {
            0              => Some("(No error)"),
            e if e < START => errname(e),
            e              => errcode(e).map(|(name, _)| name),
        };
        name.unwrap_or("(Invalid error)")
    }

    fn chain(&self) -> impl Iterator<Item = u32> {
        iter::successors(Some(self.0.unsigned_abs()), |&e| {
            errcode(e).map(|(_, parent)| parent).filter(|&p| p != 0)
        })
    }

    pub fn matches(&self, class: impl Into<BchError>) -> bool {
        let class = class.into().0.unsigned_abs();
        self.0 != 0 && self.chain().any(|e| e == class)
    }

    pub fn errno(&self) -> i32 {
        self.chain().last().unwrap() as i32
    }
}

/// For a lookup where not finding anything is a normal answer: found() turns
/// ENOENT - and every code derived from it - into Ok(None).
pub trait Found<T> {
    fn found(self) -> Result<Option<T>, BchError>;
}

impl<T> Found<T> for Result<T, BchError> {
    fn found(self) -> Result<Option<T>, BchError> {
        match self {
            Ok(v)                           => Ok(Some(v)),
            Err(e) if e.matches(c::ENOENT)  => Ok(None),
            Err(e)                          => Err(e),
        }
    }
}

impl fmt::Display for BchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg())
    }
}

impl fmt::Debug for BchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BchError({}, {})", self.0, self)
    }
}

impl core::error::Error for BchError {}

/// A failed syscall, typed. bcachefs's errcodes reach userspace intact -
/// BCH_ERR_START plus a pinned number, below MAX_ERRNO - so an io::Error off a
/// bcachefs syscall is worth converting: bch2_fs_get_tree() deliberately
/// doesn't bch2_err_class(). No errno behind it, and it wasn't a syscall: EIO,
/// which nothing matches by accident.
#[cfg(feature = "std")]
impl From<std::io::Error> for BchError {
    fn from(e: std::io::Error) -> BchError {
        const EIO: i32 = 5;

        BchError::from_raw(e.raw_os_error().unwrap_or(EIO))
    }
}

/// What can't fail - an infallible initializer, where a fallible one goes.
impl From<core::convert::Infallible> for BchError {
    fn from(e: core::convert::Infallible) -> BchError {
        match e {}
    }
}

/// A failed allocation is a bare -ENOMEM, as darray_push()'s is: callers that
/// want to say where map it to their own ENOMEM_* code.
impl From<crate::util::alloc::AllocError> for BchError {
    fn from(_: crate::util::alloc::AllocError) -> BchError {
        c::ENOMEM.into()
    }
}

/// The kernel crate's errors, a negative errno: from its APIs - uaccess's
/// EFAULT, say.
#[cfg(kernel)]
impl From<kernel::error::Error> for BchError {
    fn from(e: kernel::error::Error) -> BchError {
        BchError(-e.to_errno())
    }
}

/// An error set from anywhere, once: the first set is kept, later ones
/// dropped - why something was cancelled, or what a fork-join's jobs ran
/// into first.
pub struct FirstErr(AtomicI32);

impl FirstErr {
    pub const fn new() -> Self {
        FirstErr(AtomicI32::new(0))
    }

    /// @e, unless there's one already.
    pub fn set(&self, e: impl Into<BchError>) {
        let _ = self.0.compare_exchange(0, e.into().raw(), Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn get(&self) -> Option<BchError> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            e => Some(BchError(e)),
        }
    }

    pub fn result(&self) -> Result<(), BchError> {
        self.get().map_or(Ok(()), Err)
    }
}

impl Default for FirstErr {
    fn default() -> Self {
        Self::new()
    }
}

pub fn ret_to_result(ret: c_int) -> Result<c_int, BchError> {
    if ret < 0 && ret > -4096 {
        Err(BchError(-ret))
    } else {
        Ok(ret)
    }
}

pub fn ret_to_result_void(ret: c_int) -> Result<(), BchError> {
    ret_to_result(ret).map(|_| ())
}

/// ret_to_result(), of a loff_t: a position, or a -errcode.
pub fn loff_to_result(ret: i64) -> Result<u64, BchError> {
    if ret < 0 && ret > -4096 {
        Err(BchError(-ret as c_int))
    } else {
        Ok(ret as u64)
    }
}

/// The other way: a result as C's int, 0 or -errcode - for Rust that C calls.
pub fn ret_to_c<E: Into<BchError>>(ret: Result<(), E>) -> c_int {
    match ret {
        Ok(())  => 0,
        Err(e)  => -e.into().raw(),
    }
}

/// ret_to_c(), for the VFS: 0, or the plain errno class.
pub fn ret_to_class<E: Into<BchError>>(ret: Result<(), E>) -> c_int {
    ret.map_or_else(|e| e.into().class(), |()| 0)
}

/// A pointer from C that's an ERR_PTR() on failure, and never NULL: its
/// error, or it. An ERR_PTR() is a -errno as a pointer - one of the top
/// MAX_ERRNO (4095) addresses.
pub fn errptr_to_result<T>(p: NonNull<T>) -> Result<NonNull<T>, BchError> {
    let addr = p.as_ptr() as usize;
    if addr > usize::MAX - 4095 {
        Err(BchError(-(addr as i32)))
    } else {
        Ok(p)
    }
}

/// One that may be NULL too, as IS_ERR_OR_NULL() tests: None for NULL.
pub fn errptr_or_null<T>(p: *mut T) -> Result<Option<NonNull<T>>, BchError> {
    NonNull::new(p).map(errptr_to_result).transpose()
}

/// ERR_PTR(): @e as the pointer C returns for it.
pub fn err_ptr<T>(e: BchError) -> *mut T {
    (-(e.raw() as isize)) as *mut T
}
