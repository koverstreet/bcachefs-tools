/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ERRCODE_H
#define _BCACHEFS_ERRCODE_H

/* we're getting away from reusing bi_status, this should go away */
#define BLK_STS_REMOVED		((__force blk_status_t)128)

#define BLK_ERRS()				\
	BLK_STS(NOTSUPP, 1)			\
	BLK_STS(TIMEOUT, 2)			\
	BLK_STS(NOSPC, 3)			\
	BLK_STS(TRANSPORT, 4)			\
	BLK_STS(TARGET, 5)			\
	BLK_STS(RESV_CONFLICT, 6)		\
	BLK_STS(MEDIUM, 7)			\
	BLK_STS(PROTECTION, 8)			\
	BLK_STS(RESOURCE, 9)			\
	BLK_STS(IOERR, 10)			\
	BLK_STS(DM_REQUEUE, 11)			\
	BLK_STS(AGAIN, 12)			\
	BLK_STS(DEV_RESOURCE, 13)		\
	BLK_STS(ZONE_OPEN_RESOURCE, 14)		\
	BLK_STS(ZONE_ACTIVE_RESOURCE, 15)	\
	BLK_STS(OFFLINE, 16)			\
	BLK_STS(DURATION_LIMIT, 17)		\
	BLK_STS(INVAL, 18)			\
	BLK_STS(REMOVED, 19)			\

#define BLK_STS(n, nr)				\
	x(BCH_ERR_blockdev_io_error,	BLK_STS_##n, nr)

#define ZSTD_ERRS()					\
	ZSTD_error(GENERIC, 508)			\
	ZSTD_error(prefix_unknown, 509)			\
	ZSTD_error(version_unsupported, 510)		\
	ZSTD_error(frameParameter_unsupported, 511)	\
	ZSTD_error(frameParameter_windowTooLarge, 512)	\
	ZSTD_error(corruption_detected, 513)		\
	ZSTD_error(checksum_wrong, 514)			\
	ZSTD_error(dictionary_corrupted, 515)		\
	ZSTD_error(dictionary_wrong, 516)		\
	ZSTD_error(dictionaryCreation_failed, 517)	\
	ZSTD_error(parameter_unsupported, 518)		\
	ZSTD_error(parameter_outOfBound, 519)		\
	ZSTD_error(tableLog_tooLarge, 520)		\
	ZSTD_error(maxSymbolValue_tooLarge, 521)	\
	ZSTD_error(maxSymbolValue_tooSmall, 522)	\
	ZSTD_error(stage_wrong, 523)			\
	ZSTD_error(init_missing, 524)			\
	ZSTD_error(memory_allocation, 525)		\
	ZSTD_error(workSpace_tooSmall, 526)		\
	ZSTD_error(dstSize_tooSmall, 527)		\
	ZSTD_error(srcSize_wrong, 528)			\
	ZSTD_error(dstBuffer_null, 529)			\
	ZSTD_error(frameIndex_tooLarge, 530)		\
	ZSTD_error(seekableIO, 531)			\
	ZSTD_error(dstBuffer_wrong, 532)		\
	ZSTD_error(srcBuffer_wrong, 533)

#define ZSTD_error(n, nr)					\
	x(BCH_ERR_zstd_error,	ZSTD_error_##n, nr)

#define BCH_ERRCODES()								\
	x(EIO,				blockdev_io_error, 0)			\
	BLK_ERRS()								\
	x(BCH_ERR_blockdev_io_error,	BLK_STS_UNKNOWN, 20)			\
	x(ERANGE,			ERANGE_option_too_small, 21)		\
	x(ERANGE,			ERANGE_option_too_big, 22)		\
	x(ERANGE,			projid_too_big, 23)			\
	x(EINVAL,			injected, 24)				\
	x(BCH_ERR_injected,		injected_fs_start, 25)			\
	x(EINVAL,			mount_option, 26)			\
	x(BCH_ERR_mount_option,		option_name, 27)			\
	x(BCH_ERR_mount_option,		option_value, 28)			\
	x(BCH_ERR_mount_option,         option_not_bool, 29)			\
	x(ENOMEM,			ENOMEM_stripe_buf, 30)			\
	x(ENOMEM,			ENOMEM_replicas_table, 31)		\
	x(ENOMEM,			ENOMEM_cpu_replicas, 32)		\
	x(ENOMEM,			ENOMEM_replicas_gc, 33)			\
	x(ENOMEM,			ENOMEM_disk_groups_validate, 34)	\
	x(ENOMEM,			ENOMEM_disk_groups_to_cpu, 35)		\
	x(ENOMEM,			ENOMEM_mark_snapshot, 36)		\
	x(ENOMEM,			ENOMEM_mark_stripe, 37)			\
	x(ENOMEM,			ENOMEM_mark_stripe_ptr, 38)		\
	x(ENOMEM,			ENOMEM_btree_key_cache_create, 39)	\
	x(ENOMEM,			ENOMEM_btree_key_cache_fill, 40)	\
	x(ENOMEM,			ENOMEM_btree_key_cache_insert, 41)	\
	x(ENOMEM,			ENOMEM_trans_kmalloc, 42)		\
	x(ENOMEM,			ENOMEM_trans_log_msg, 43)		\
	x(ENOMEM,			ENOMEM_do_encrypt, 44)			\
	x(ENOMEM,			ENOMEM_ec_read_extent, 45)		\
	x(ENOMEM,			ENOMEM_ec_stripe_mem_alloc, 46)		\
	x(ENOMEM,			ENOMEM_ec_new_stripe_alloc, 47)		\
	x(ENOMEM,			ENOMEM_fs_btree_cache_init, 48)		\
	x(ENOMEM,			ENOMEM_fs_btree_key_cache_init, 49)	\
	x(ENOMEM,			ENOMEM_fs_counters_init, 50)		\
	x(ENOMEM,			ENOMEM_fs_btree_write_buffer_init, 51)	\
	x(ENOMEM,			ENOMEM_io_clock_init, 52)		\
	x(ENOMEM,			ENOMEM_blacklist_table_init, 53)	\
	x(ENOMEM,			ENOMEM_sb_realloc_injected, 54)		\
	x(ENOMEM,			ENOMEM_sb_bio_realloc, 55)		\
	x(ENOMEM,			ENOMEM_sb_buf_realloc, 56)		\
	x(ENOMEM,			ENOMEM_sb_journal_validate, 57)		\
	x(ENOMEM,			ENOMEM_sb_journal_v2_validate, 58)	\
	x(ENOMEM,			ENOMEM_journal_entry_add, 59)		\
	x(ENOMEM,			ENOMEM_journal_read_buf_realloc, 60)	\
	x(ENOMEM,			ENOMEM_btree_interior_update_worker_init, 61) \
	x(ENOMEM,			ENOMEM_btree_node_rewrites_table_init, 62) \
	x(ENOMEM,			ENOMEM_btree_interior_update_pool_init, 63) \
	x(ENOMEM,			ENOMEM_bio_read_init, 64)		\
	x(ENOMEM,			ENOMEM_bio_read_split_init, 65)		\
	x(ENOMEM,			ENOMEM_bio_write_init, 66)		\
	x(ENOMEM,			ENOMEM_promote_limit_init, 67)		\
	x(ENOMEM,			ENOMEM_bio_bounce_pages_init, 68)	\
	x(ENOMEM,			ENOMEM_writepage_bioset_init, 69)	\
	x(ENOMEM,			ENOMEM_writepage_buf_pool_init, 70)	\
	x(ENOMEM,			ENOMEM_dio_read_bioset_init, 71)	\
	x(ENOMEM,			ENOMEM_dio_write_bioset_init, 72)	\
	x(ENOMEM,			ENOMEM_nocow_flush_bioset_init, 73)	\
	x(ENOMEM,			ENOMEM_promote_table_init, 74)		\
	x(ENOMEM,			ENOMEM_async_obj_init, 75)		\
	x(ENOMEM,			ENOMEM_compression_bounce_read_init, 76) \
	x(ENOMEM,			ENOMEM_compression_bounce_write_init, 77) \
	x(ENOMEM,			ENOMEM_compression_workspace_init, 78)	\
	x(ENOMEM,			ENOMEM_backpointer_mismatches_bitmap, 79) \
	x(EIO,				compression_workspace_not_initialized, 80) \
	x(ENOMEM,			ENOMEM_bucket_gens, 81)			\
	x(ENOMEM,			ENOMEM_buckets_nouse, 82)		\
	x(ENOMEM,			ENOMEM_usage_init, 83)			\
	x(ENOMEM,			ENOMEM_btree_node_read_all_replicas, 84) \
	x(ENOMEM,			ENOMEM_btree_node_reclaim, 85)		\
	x(ENOMEM,			ENOMEM_btree_node_mem_alloc, 86)	\
	x(ENOMEM,			ENOMEM_btree_cache_cannibalize_lock, 87) \
	x(ENOMEM,			ENOMEM_set_nr_journal_buckets, 88)	\
	x(ENOMEM,			ENOMEM_dev_journal_init, 89)		\
	x(ENOMEM,			ENOMEM_journal_pin_fifo, 90)		\
	x(ENOMEM,			ENOMEM_journal_buf, 91)			\
	x(ENOMEM,			ENOMEM_gc_start, 92)			\
	x(ENOMEM,			ENOMEM_gc_alloc_start, 93)		\
	x(ENOMEM,			ENOMEM_gc_reflink_start, 94)		\
	x(ENOMEM,			ENOMEM_gc_gens, 95)			\
	x(ENOMEM,			ENOMEM_gc_repair_key, 96)		\
	x(ENOMEM,			ENOMEM_fsck_extent_ends_at, 97)		\
	x(ENOMEM,			ENOMEM_fsck_add_nlink, 98)		\
	x(ENOMEM,			ENOMEM_journal_key_insert, 99)		\
	x(ENOMEM,			ENOMEM_journal_keys_sort, 100)		\
	x(ENOMEM,			ENOMEM_read_superblock_clean, 101)	\
	x(ENOMEM,			ENOMEM_fs_alloc, 102)			\
	x(ENOMEM,			ENOMEM_fs_name_alloc, 103)		\
	x(ENOMEM,			ENOMEM_fs_other_alloc, 104)		\
	x(ENOMEM,			ENOMEM_dev_alloc, 105)			\
	x(ENOMEM,			ENOMEM_disk_accounting, 106)		\
	x(ENOMEM,			ENOMEM_stripe_head_alloc, 107)		\
	x(ENOMEM,                       ENOMEM_journal_read_bucket, 108)	\
	x(ENOMEM,                       ENOMEM_acl, 109)			\
	x(ENOMEM,                       ENOMEM_move_extent, 110)		\
	x(ENOMEM,			ENOMEM_reconcile_scan_in_flight, 111)	\
	x(ENOSPC,			ENOSPC_disk_reservation, 112)		\
	x(ENOSPC,			ENOSPC_bucket_alloc, 113)		\
	x(ENOSPC,			ENOSPC_disk_label_add, 114)		\
	x(ENOSPC,			ENOSPC_stripe_create, 115)		\
	x(ENOSPC,			ENOSPC_inode_create, 116)		\
	x(ENOSPC,			ENOSPC_str_hash_create, 117)		\
	x(ENOSPC,			ENOSPC_snapshot_create, 118)		\
	x(ENOSPC,			ENOSPC_subvolume_create, 119)		\
	x(ENOSPC,			ENOSPC_sb, 120)				\
	x(ENOSPC,			ENOSPC_sb_journal, 121)			\
	x(ENOSPC,			ENOSPC_sb_journal_seq_blacklist, 122)	\
	x(ENOSPC,			ENOSPC_sb_quota, 123)			\
	x(ENOSPC,			ENOSPC_sb_replicas, 124)		\
	x(ENOSPC,			ENOSPC_sb_members, 125)			\
	x(ENOSPC,			ENOSPC_sb_members_v2, 126)		\
	x(ENOSPC,			ENOSPC_sb_extent_type_u64s, 127)	\
	x(ENOSPC,			ENOSPC_sb_crypt, 128)			\
	x(ENOSPC,			ENOSPC_sb_downgrade, 129)		\
	x(ENOSPC,			ENOSPC_btree_slot, 130)			\
	x(ENOSPC,			ENOSPC_snapshot_tree, 131)		\
	x(ENOENT,			ENOENT_bkey_type_mismatch, 132)		\
	x(ENOENT,			ENOENT_str_hash_lookup, 133)		\
	x(ENOENT,			ENOENT_str_hash_set_must_replace, 134)	\
	x(ENOENT,			ENOENT_inode, 135)			\
	x(ENOENT,			ENOENT_not_subvol, 136)			\
	x(ENOENT,			ENOENT_not_directory, 137)		\
	x(ENOENT,			ENOENT_directory_dead, 138)		\
	x(ENOENT,			ENOENT_subvolume, 139)			\
	x(ENOENT,			ENOENT_subvolume_deleted, 140)		\
	x(ENOENT,			ENOENT_snapshot, 141)			\
	x(ENOENT,			ENOENT_snapshot_tree, 142)		\
	x(ENOENT,			ENOENT_dirent_doesnt_match_inode, 143)	\
	x(ENOENT,			ENOENT_dev_not_found, 144)		\
	x(ENOENT,			ENOENT_dev_bucket_not_found, 145)	\
	x(ENOENT,			ENOENT_dev_idx_not_found, 146)		\
	x(ENOENT,			ENOENT_inode_no_backpointer, 147)	\
	x(ENOENT,			ENOENT_no_snapshot_tree_subvol, 148)	\
	x(ENOENT,			btree_node_dying, 149)			\
	x(ENOTEMPTY,			ENOTEMPTY_dir_not_empty, 150)		\
	x(ENOTEMPTY,			ENOTEMPTY_subvol_not_empty, 151)	\
	x(EEXIST,			EEXIST_str_hash_set, 152)		\
	x(EEXIST,			EEXIST_discard_in_flight_add, 153)	\
	x(EEXIST,			EEXIST_subvolume_create, 154)		\
	x(EAGAIN,			open_buckets_empty, 155)		\
	x(EAGAIN,			freelist_empty, 156)			\
	x(EAGAIN,			stripe_needs_block_evacuate, 157)	\
	x(EAGAIN,			stripe_insufficient_devices, 158)	\
	x(EAGAIN,			max_discards_in_flight, 159)		\
	x(ENOSPC,			ec_alloc_failed, 160)			\
	x(BCH_ERR_freelist_empty,	no_buckets_found, 161)			\
	x(BCH_ERR_freelist_empty,	bucket_alloc_no_progress, 162)		\
	x(0,				transaction_restart, 163)		\
	x(BCH_ERR_transaction_restart,	transaction_restart_fault_inject, 164)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_relock, 165)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_relock_path, 166)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_relock_path_intent, 167) \
	x(BCH_ERR_transaction_restart,	transaction_restart_too_many_iters, 168) \
	x(BCH_ERR_transaction_restart,	transaction_restart_lock_node_reused, 169) \
	x(BCH_ERR_transaction_restart,	transaction_restart_fill_relock, 170)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_fill_mem_alloc_fail, 171) \
	x(BCH_ERR_transaction_restart,	transaction_restart_lock_waitlist_alloc, 172) \
	x(BCH_ERR_transaction_restart,	transaction_restart_mem_realloced, 173)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_in_traverse_all, 174) \
	x(BCH_ERR_transaction_restart,	transaction_restart_would_deadlock, 175) \
	x(BCH_ERR_transaction_restart,	transaction_restart_would_deadlock_write, 176) \
	x(BCH_ERR_transaction_restart,	transaction_restart_deadlock_recursion_limit, 177) \
	x(BCH_ERR_transaction_restart,	transaction_restart_deadlock_waitlist_alloc, 178) \
	x(BCH_ERR_transaction_restart,	transaction_restart_upgrade, 179)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_key_cache_fill, 180) \
	x(BCH_ERR_transaction_restart,	transaction_restart_key_cache_raced, 181) \
	x(BCH_ERR_transaction_restart,	transaction_restart_lock_root_race, 182) \
	x(BCH_ERR_transaction_restart,	transaction_restart_split_race, 183)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_split_with_interior_updates, 184) \
	x(BCH_ERR_transaction_restart,	transaction_restart_write_buffer_flush, 185) \
	x(BCH_ERR_transaction_restart,	transaction_restart_nested, 186)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_commit, 187)	\
	x(BCH_ERR_transaction_restart,	transaction_restart_journal_overwrites_changed, 188) \
	x(0,				no_btree_node, 189)			\
	x(BCH_ERR_no_btree_node,	no_btree_node_relock, 190)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_upgrade, 191)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_drop, 192)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_lock_root, 193)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_up, 194)			\
	x(BCH_ERR_no_btree_node,	no_btree_node_down, 195)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_init, 196)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_cached, 197)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_srcu_reset, 198)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_nofill, 199)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_reused, 200)		\
	x(BCH_ERR_no_btree_node,	no_btree_node_stale_paths, 201)		\
	x(0,				btree_insert_fail, 202)			\
	x(BCH_ERR_btree_insert_fail,	btree_insert_btree_node_full, 203)	\
	x(BCH_ERR_btree_insert_fail,	btree_insert_need_mark_replicas, 204)	\
	x(BCH_ERR_btree_insert_fail,	btree_insert_need_journal_res, 205)	\
	x(BCH_ERR_btree_insert_fail,	btree_insert_need_journal_reclaim, 206)	\
	x(0,				backpointer_to_overwritten_btree_node, 207) \
	x(0,				journal_reclaim_would_deadlock, 208)	\
	x(0,				journal_rewind_no_overwrites, 209)	\
	x(EROFS,			fsck, 210)				\
	x(BCH_ERR_fsck,			fsck_ask, 211)				\
	x(BCH_ERR_fsck,			fsck_fix, 212)				\
	x(BCH_ERR_fsck,			fsck_delete_bkey, 213)			\
	x(BCH_ERR_fsck,			fsck_ignore, 214)			\
	x(BCH_ERR_fsck,			fsck_errors_not_fixed, 215)		\
	x(BCH_ERR_fsck,			fsck_repair_unimplemented, 216)		\
	x(BCH_ERR_fsck,			fsck_repair_impossible, 217)		\
	x(EINVAL,			recovery_will_run, 218)			\
	x(BCH_ERR_recovery_will_run,	restart_recovery, 219)			\
	x(BCH_ERR_recovery_will_run,	cannot_rewind_recovery, 220)		\
	x(BCH_ERR_recovery_will_run,	recovery_pass_will_run, 221)		\
	x(0,				bkey_was_deleted, 222)			\
	x(0,				bucket_not_moveable, 223)		\
	x(BCH_ERR_bucket_not_moveable,	bucket_not_moveable_dev_not_rw, 224)	\
	x(BCH_ERR_bucket_not_moveable,	bucket_not_moveable_bucket_open, 225)	\
	x(BCH_ERR_bucket_not_moveable,	bucket_not_moveable_bp_mismatch, 226)	\
	x(BCH_ERR_bucket_not_moveable,	bucket_not_moveable_lru_race, 227)	\
	x(0,				data_update_done, 228)			\
	x(BCH_ERR_data_update_done,	data_update_done_unwritten, 229)	\
	x(BCH_ERR_data_update_done,	data_update_done_no_writes_needed, 230)	\
	x(0,				data_update_fail, 231)			\
	x(BCH_ERR_data_update_fail,	data_update_fail_would_block, 232)	\
	x(BCH_ERR_data_update_fail,	data_update_fail_in_flight, 233)	\
	x(BCH_ERR_data_update_fail,	data_update_fail_no_snapshot, 234)	\
	x(BCH_ERR_data_update_fail,	data_update_fail_no_rw_devs, 235)	\
	x(BCH_ERR_data_update_fail,	data_update_fail_need_copygc, 236)	\
	x(EPERM,			reflink_p_may_update_options_unset, 237) \
	x(EPERM,			EPERM_non_admin, 238)			\
	x(EPERM,			EPERM_non_admin_or_owner, 239)		\
	x(EINVAL,			device_state_not_allowed, 240)		\
	x(EINVAL,			member_info_missing, 241)		\
	x(EINVAL,			mismatched_block_size, 242)		\
	x(EINVAL,			block_size_too_small, 243)		\
	x(EINVAL,			bucket_size_too_small, 244)		\
	x(EINVAL,			device_size_too_small, 245)		\
	x(EINVAL,			device_size_too_big, 246)		\
	x(EINVAL,			device_not_a_member_of_filesystem, 247)	\
	x(EINVAL,			device_has_been_removed, 248)		\
	x(EINVAL,			device_splitbrain, 249)			\
	x(EINVAL,			device_already_online, 250)		\
	x(EINVAL,			filesystem_uuid_already_open, 251)	\
	x(EINVAL,			insufficient_devices_to_start, 252)	\
	x(EINVAL,			chardev_init_error, 253)		\
	x(EINVAL,			sysfs_init_error, 254)			\
	x(EINVAL,			invalid, 255)				\
	x(EINVAL,			internal_fsck_err, 256)			\
	x(EINVAL,			opt_parse_error, 257)			\
	x(EINVAL,			remove_with_metadata_missing_unimplemented, 258) \
	x(EINVAL,			remove_would_lose_data, 259)		\
	x(EINVAL,			remove_by_backpointer_did_not_terminate, 260) \
	x(EINVAL,			remove_stripes_did_not_terminate, 261) \
	x(EINVAL,			no_resize_with_buckets_nouse, 262)	\
	x(EINVAL,			inode_unpack_error, 263)		\
	x(EINVAL,			inode_not_unlinked, 264)		\
	x(EINVAL,			inode_has_child_snapshot, 265)		\
	x(EINVAL,			inode_is_subvolume_root, 266)		\
	x(EINVAL,			varint_decode_error, 267)		\
	x(EINVAL,			erasure_coding_found_btree_node, 268)	\
	x(EINVAL,			erasure_coding_stripe_update_err, 269)	\
	x(EINVAL,			stripe_unknown_csum_type, 270)		\
	x(EINVAL,			option_negative, 271)			\
	x(EINVAL,			topology_repair, 272)			\
	x(EINVAL,			EINVAL_unaligned_io, 273)		\
	x(EINVAL,			EINVAL_rename_bad_flags, 274)		\
	x(EINVAL,			EINVAL_setattr_bad_file_type, 275)	\
	x(EINVAL,			EINVAL_get_name_not_dir, 276)		\
	x(EINVAL,			EINVAL_reconfigure_read_write, 277)	\
	x(EINVAL,			EINVAL_fcollapse_finsert_unaligned, 278) \
	x(EINVAL,			EINVAL_finsert_past_eof, 279)		\
	x(EINVAL,			EINVAL_fcollapse_past_eof, 280)		\
	x(EINVAL,			EINVAL_remap_bad_flags, 281)		\
	x(EINVAL,			EINVAL_remap_unaligned, 282)		\
	x(EINVAL,			EINVAL_remap_overlapping, 283)		\
	x(EINVAL,			EINVAL_setlabel_too_long, 284)		\
	x(EINVAL,			EINVAL_goingdown_bad_flags, 285)	\
	x(EINVAL,			EINVAL_subvol_create_bad_flags, 286)	\
	x(EINVAL,			EINVAL_subvol_create_flags_mismatch, 287) \
	x(EINVAL,			EINVAL_subvol_destroy_bad_flags, 288)	\
	x(EINVAL,			EINVAL_subvol_readdir_pad, 289)		\
	x(EINVAL,			EINVAL_subvol_to_path_no_buf, 290)	\
	x(EINVAL,			EINVAL_snapshot_tree_query_pad, 291)	\
	x(EINVAL,			EINVAL_fiemap_overflow, 292)		\
	x(EINVAL,			EINVAL_snapshot_not_subvol_root, 293)	\
	x(EINVAL,			EINVAL_quota_enable_acct, 294)		\
	x(EINVAL,			EINVAL_quota_enable_usrquota, 295)	\
	x(EINVAL,			EINVAL_quota_enable_grpquota, 296)	\
	x(EINVAL,			EINVAL_quota_enable_prjquota, 297)	\
	x(EINVAL,			EINVAL_quota_remove_usrquota, 298)	\
	x(EINVAL,			EINVAL_quota_remove_grpquota, 299)	\
	x(EINVAL,			EINVAL_quota_remove_prjquota, 300)	\
	x(EINVAL,			EINVAL_quota_set_info_bad_type, 301)	\
	x(EINVAL,			EINVAL_quota_set_info_bad_field, 302)	\
	x(EINVAL,			EINVAL_xattr_get_bad_opt, 303)		\
	x(EINVAL,			EINVAL_xattr_get_not_inode_opt, 304)	\
	x(EINVAL,			EINVAL_xattr_set_bad_opt, 305)		\
	x(EINVAL,			EINVAL_xattr_set_not_inode_opt, 306)	\
	x(EINVAL,			EINVAL_fsck_offline_bad_flags, 307)	\
	x(EINVAL,			EINVAL_fsck_online_bad_passes, 308)	\
	x(EINVAL,			EINVAL_fsck_online_bad_flags, 309)	\
	x(EINVAL,			EINVAL_journal_replay_key_bad_btree_depth, 310) \
	x(EINVAL,			EINVAL_dev_resize_shrink, 311)		\
	x(EINVAL,			EINVAL_missing_new_extent_overwrite, 312) \
	x(EINVAL,			EINVAL_version_min_too_old, 313)	\
	x(EINVAL,			EINVAL_block_size_needs_thp, 314)	\
	x(EINVAL,			EINVAL_utf8_load_failed, 315)		\
	x(EINVAL,			EINVAL_casefolding_no_unicode, 316)	\
	x(EINVAL,			EINVAL_no_version_check_start, 317)	\
	x(EINVAL,			EINVAL_ioctl_disk_add_bad_flags, 318)	\
	x(EINVAL,			EINVAL_ioctl_disk_add_v2_bad_flags, 319) \
	x(EINVAL,			EINVAL_ioctl_disk_remove_bad_flags, 320) \
	x(EINVAL,			EINVAL_ioctl_disk_remove_v2_bad_flags, 321) \
	x(EINVAL,			EINVAL_ioctl_disk_online_bad_flags, 322) \
	x(EINVAL,			EINVAL_ioctl_disk_online_v2_bad_flags, 323) \
	x(EINVAL,			EINVAL_ioctl_disk_offline_bad_flags, 324) \
	x(EINVAL,			EINVAL_ioctl_disk_offline_v2_bad_flags, 325) \
	x(EINVAL,			EINVAL_ioctl_disk_set_state_bad_args, 326) \
	x(EINVAL,			EINVAL_ioctl_disk_set_state_v2_bad_args, 327) \
	x(EINVAL,			EINVAL_ioctl_data_read_short_buf, 328)	\
	x(EINVAL,			EINVAL_ioctl_data_bad_op, 329)		\
	x(EINVAL,			EINVAL_ioctl_fs_usage_accounting_not_read, 330)	\
	x(EINVAL,			EINVAL_ioctl_query_accounting_not_read, 331)	\
	x(EINVAL,			EINVAL_ioctl_dev_usage_accounting_not_read, 332)	\
	x(EINVAL,			EINVAL_ioctl_dev_usage_bad_flags, 333)		\
	x(EINVAL,			EINVAL_ioctl_dev_usage_v2_accounting_not_read, 334)	\
	x(EINVAL,			EINVAL_ioctl_dev_usage_v2_bad_flags, 335)	\
	x(EINVAL,			EINVAL_ioctl_query_btree_keys_bad_flags, 336)	\
	x(EINVAL,			EINVAL_ioctl_query_btree_keys_bad_params, 337)	\
	x(EINVAL,			EINVAL_ioctl_read_super_bad_flags, 338)		\
	x(EINVAL,			EINVAL_ioctl_disk_get_idx_bad_dev, 339)		\
	x(EINVAL,			EINVAL_ioctl_disk_resize_bad_flags, 340)	\
	x(EINVAL,			EINVAL_ioctl_disk_resize_v2_bad_flags, 341)	\
	x(EINVAL,			EINVAL_ioctl_disk_resize_journal_bad_flags, 342) \
	x(EINVAL,			EINVAL_ioctl_disk_resize_journal_too_big, 343)	\
	x(EINVAL,			EINVAL_ioctl_disk_resize_journal_v2_bad_flags, 344) \
	x(EINVAL,			EINVAL_ioctl_disk_resize_journal_v2_too_big, 345) \
	x(EINVAL,			EINVAL_ioctl_not_started, 346)			\
	x(EINVAL,			EINVAL_journal_write_overran_available_space, 347) \
	x(EINVAL,			EINVAL_journal_bucket_not_found, 348)		\
	x(EINVAL,			EINVAL_journal_seq_overflow, 349)		\
	x(EINVAL,			EINVAL_journal_entry_version_incompatible, 350)	\
	x(EINVAL,			EINVAL_journal_validate_version_incompatible, 351) \
	x(EINVAL,			EINVAL_journal_rewind_before_discard, 352)	\
	x(EINVAL,			EINVAL_opt_target_parse_not_found, 353)		\
	x(EINVAL,			EINVAL_disable_encryption_no_crypt, 354)	\
	x(EINVAL,			EINVAL_reflink_gc_table_mismatch, 355)		\
	x(EINVAL,			EINVAL_ec_stripe_create_existing_key, 356)	\
	x(EINVAL,			EINVAL_finsert_offset_past_eof, 357)		\
	x(EINVAL,			EINVAL_data_job_bad_op, 358)			\
	x(EINVAL,			EINVAL_snapshot_parent_already_has_children, 359) \
	x(EINVAL,			EINVAL_snapshot_delete_has_two_children, 360)	\
	x(EINVAL,			EINVAL_snapshot_delete_interior_at_runtime, 361) \
	x(EINVAL,			EINVAL_snapshot_delete_with_data, 362)		\
	x(EINVAL,			EINVAL_snapshot_delete_already_deleted, 363)	\
	x(EINVAL,			EINVAL_snapshot_delete_bad_subvol, 364)		\
	x(EINVAL,			EINVAL_snapshot_delete_bad_topology, 365)	\
	x(EINVAL,			EINVAL_snapshot_parent_missing_child_ptr, 366)	\
	x(EINVAL,			EINVAL_snapshot_child_bad_parent, 367)		\
	x(EINVAL,			EINVAL_snapshot_edge_to_missing_node, 368)	\
	x(EINVAL,			EINVAL_snapshot_bad_subvol_flag, 369)		\
	x(EINVAL,			EINVAL_opt_parse_uint_required, 370)	\
	x(EINVAL,			EINVAL_opt_parse_str_required, 371)	\
	x(EINVAL,			EINVAL_test_zero_nr_or_threads, 372)	\
	x(EINVAL,			EINVAL_test_unknown_test, 373)		\
	x(EINVAL,			EINVAL_sysfs_opt_not_found, 374)	\
	x(EINVAL,			EINVAL_ioctl_query_counters_bad_flags, 375) \
	x(EINVAL,			EINVAL_node_scan_no_nodes, 376)		\
	x(EINVAL,			EINVAL_node_scan_too_many_replicas, 377) \
	x(EINVAL,			EINVAL_parse_btree_id, 378)		\
	x(EINVAL,			EINVAL_parse_bkey_type, 379)		\
	x(EINVAL,			EINVAL_parse_bpos, 380)			\
	x(EINVAL,			EINVAL_parse_bbpos, 381)		\
	x(BCH_ERR_topology_repair,	topology_repair_drop_this_node, 382)	\
	x(BCH_ERR_topology_repair,	topology_repair_drop_prev_node, 383)	\
	x(BCH_ERR_topology_repair,	topology_repair_did_fill_from_scan, 384) \
	x(EMLINK,			too_many_links, 385)			\
	x(EOPNOTSUPP,			may_not_use_incompat_feature, 386)	\
	x(EOPNOTSUPP,			no_casefolding_without_utf8, 387)	\
	x(EOPNOTSUPP,			casefolding_disabled, 388)		\
	x(EOPNOTSUPP,			casefold_opt_is_dir_only, 389)		\
	x(EOPNOTSUPP,			casefolding_in_use, 390)		\
	x(EOPNOTSUPP,			casefold_dir_but_disabled, 391)		\
	x(EOPNOTSUPP,			unsupported_fsx_flag, 392)		\
	x(EOPNOTSUPP,			unsupported_fa_flag, 393)		\
	x(EOPNOTSUPP,			unsupported_fallocate_mode, 394)	\
	x(EROFS,			erofs_trans_commit, 395)		\
	x(EROFS,			erofs_no_writes, 396)			\
	x(EROFS,			erofs_journal_err, 397)			\
	x(EROFS,			erofs_sb_err, 398)			\
	x(EROFS,			erofs_unfixed_errors, 399)		\
	x(EROFS,			erofs_norecovery, 400)			\
	x(EROFS,			erofs_nochanges, 401)			\
	x(EROFS,			erofs_no_alloc_info, 402)		\
	x(EROFS,			erofs_filesystem_full, 403)		\
	x(EROFS,			erofs_sb_not_migrated, 404)		\
	x(EROFS,			insufficient_devices, 405)		\
	x(EROFS,			erofs_recovery_cancelled, 406)		\
	x(EROFS,			emergency_ro, 407)			\
	x(ESHUTDOWN,			btree_not_started, 408)			\
	x(0,				operation_blocked, 409)			\
	x(BCH_ERR_operation_blocked,	btree_cache_cannibalize_lock_blocked, 410) \
	x(BCH_ERR_operation_blocked,	journal_res_blocked, 411)		\
	x(BCH_ERR_operation_blocked,	bucket_alloc_blocked, 412)		\
	x(BCH_ERR_operation_blocked,	open_bucket_alloc_blocked, 413)		\
	x(BCH_ERR_operation_blocked,	stripe_alloc_blocked, 414)		\
	x(BCH_ERR_operation_blocked,	stripe_buf_mem_blocked, 415)		\
	x(EAGAIN,			stripe_buf_mem_limit, 416)		\
	x(BCH_ERR_journal_res_blocked,	journal_blocked, 417)			\
	x(BCH_ERR_journal_res_blocked,	journal_max_in_flight, 418)		\
	x(BCH_ERR_journal_res_blocked,	journal_max_open, 419)			\
	x(BCH_ERR_journal_res_blocked,	journal_full, 420)			\
	x(BCH_ERR_journal_res_blocked,	journal_pin_full, 421)			\
	x(BCH_ERR_journal_res_blocked,	journal_buf_enomem, 422)		\
	x(BCH_ERR_journal_res_blocked,	journal_stuck, 423)			\
	x(BCH_ERR_journal_res_blocked,	journal_retry_open, 424)		\
	x(BCH_ERR_invalid,		invalid_sb, 425)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_magic, 426)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_version, 427)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_features, 428)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_too_big, 429)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_csum_type, 430)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_csum, 431)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_block_size, 432)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_uuid, 433)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_offset, 434)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_too_many_members, 435)	\
	x(BCH_ERR_invalid_sb,		invalid_sb_dev_idx, 436)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_time_precision, 437)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_field_size, 438)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_layout, 439)			\
	x(BCH_ERR_invalid_sb_layout,	invalid_sb_layout_type, 440)		\
	x(BCH_ERR_invalid_sb_layout,	invalid_sb_layout_nr_superblocks, 441)	\
	x(BCH_ERR_invalid_sb_layout,	invalid_sb_layout_superblocks_overlap, 442) \
	x(BCH_ERR_invalid_sb_layout,    invalid_sb_layout_sb_max_size_bits, 443) \
	x(BCH_ERR_invalid_sb,		invalid_sb_members_missing, 444)	\
	x(BCH_ERR_invalid_sb,		invalid_sb_members, 445)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_disk_groups, 446)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_replicas, 447)		\
	x(BCH_ERR_invalid_sb,		invalid_replicas_entry, 448)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_journal, 449)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_journal_seq_blacklist, 450)	\
	x(BCH_ERR_invalid_sb,		invalid_sb_crypt, 451)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_clean, 452)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_quota, 453)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_errors, 454)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_opt_compression, 455)	\
	x(BCH_ERR_invalid_sb,		invalid_sb_ext, 456)			\
	x(BCH_ERR_invalid_sb,		invalid_sb_downgrade, 457)		\
	x(BCH_ERR_invalid_sb,		invalid_sb_extent_type_u64s, 458)	\
	x(BCH_ERR_invalid,		invalid_bkey, 459)			\
	x(BCH_ERR_operation_blocked,    nocow_lock_blocked, 460)		\
	x(EROFS,			journal_shutdown, 461)			\
	x(EIO,				journal_flush_err, 462)			\
	x(EIO,				journal_write_err, 463)			\
	x(EIO,				btree_node_read_err, 464)		\
	x(EIO,				btree_node_validate_err, 465)		\
	x(BCH_ERR_btree_node_read_err,	btree_node_read_err_cached, 466)	\
	x(EIO,				sb_not_downgraded, 467)			\
	x(EIO,				btree_node_write_all_failed, 468)	\
	x(EIO,				btree_node_read_error, 469)		\
	x(EIO,				btree_root_error_unset, 470)		\
	x(EIO,				btree_need_topology_repair, 471)	\
	x(EIO,				bucket_ref_update, 472)			\
	x(EIO,				trigger_alloc, 473)			\
	x(EIO,				trigger_pointer, 474)			\
	x(EIO,				trigger_stripe_pointer, 475)		\
	x(EIO,				metadata_bucket_inconsistency, 476)	\
	x(EIO,				mark_stripe, 477)			\
	x(EIO,				stripe_read, 478)			\
	x(BCH_ERR_stripe_read,		stripe_read_device_offline, 479)	\
	x(BCH_ERR_stripe_read,		stripe_read_ptr_stale, 480)		\
	x(BCH_ERR_stripe_read,		stripe_read_csum_err, 481)		\
	x(BCH_ERR_stripe_read,		stripe_reconstruct, 482)		\
	x(BCH_ERR_stripe_read,		stripe_reconstruct_enomem, 483)		\
	x(BCH_ERR_stripe_read,		stripe_reconstruct_insufficient_blocks, 484) \
	x(BCH_ERR_stripe_read,		stripe_reconstruct_stale_race, 485)	\
	x(EIO,				key_type_error, 486)			\
	x(EIO,				extent_poisoned, 487)			\
	x(EIO,				missing_indirect_extent, 488)		\
	x(EIO,				invalidate_stripe_to_dev, 489)		\
	x(EIO,				no_encryption_key, 490)			\
	x(EIO,				insufficient_journal_devices, 491)	\
	x(EIO,				device_offline, 492)			\
	x(EIO,				stripe_create_device_offline, 493)	\
	x(EROFS,			stripe_create_device_removing, 494)	\
	x(EIO,				EIO_fault_injected, 495)		\
	x(EIO,				ec_block_read, 496)			\
	x(EIO,				ec_block_write, 497)			\
	x(EIO,				recompute_checksum, 498)		\
	x(BCH_ERR_data_read_retry_avoid,decompress, 499)			\
	x(BCH_ERR_decompress,		decompress_exceeded_max_encoded_extent, 500) \
	x(BCH_ERR_decompress,		decompress_lz4_old, 501)		\
	x(BCH_ERR_decompress,		decompress_lz4, 502)			\
	x(BCH_ERR_decompress,		decompress_gzip, 503)			\
	x(BCH_ERR_decompress,		decompress_gzip_size_mismatch, 504)	\
	x(BCH_ERR_decompress,		decompress_zstd_src_len_bad, 505)	\
	x(BCH_ERR_decompress,		decompress_zstd_size_mismatch, 506)	\
	x(BCH_ERR_decompress,		zstd_error, 507)			\
	ZSTD_ERRS()								\
	x(BCH_ERR_zstd_error,		ZSTD_error_unknown, 534)		\
	x(EIO,				data_write, 535)			\
	x(BCH_ERR_data_write,		data_write_io, 536)			\
	x(BCH_ERR_data_write,		data_write_csum, 537)			\
	x(BCH_ERR_data_write,		data_write_invalid_ptr, 538)		\
	x(BCH_ERR_data_write,		data_write_misaligned, 539)		\
	x(BCH_ERR_data_write,		data_write_need_fresh_buckets, 540)	\
	x(EIO,				data_read, 541)				\
	x(BCH_ERR_data_read,		no_device_to_read_from, 542)		\
	x(BCH_ERR_data_read,		no_devices_valid, 543)			\
	x(BCH_ERR_data_read,		data_read_io_err, 544)			\
	x(BCH_ERR_data_read,		data_read_csum_err, 545)		\
	x(BCH_ERR_data_read,		data_read_retry, 546)			\
	x(BCH_ERR_data_read_retry,	data_read_retry_avoid, 547)		\
	x(BCH_ERR_data_read_retry_avoid,data_read_retry_device_offline, 548)	\
	x(BCH_ERR_data_read_retry_avoid,data_read_retry_io_err, 549)		\
	x(BCH_ERR_data_read_retry_avoid,data_read_retry_ec_reconstruct_err, 550) \
	x(BCH_ERR_data_read_retry_avoid,data_read_retry_csum_err, 551)		\
	x(BCH_ERR_data_read_retry,	data_read_retry_csum_err_maybe_userspace, 552) \
	x(BCH_ERR_data_read_retry_avoid,data_read_decompress_err, 553)		\
	x(BCH_ERR_data_read_retry_avoid,data_read_decrypt_err, 554)		\
	x(BCH_ERR_data_read,		data_read_ptr_stale_race, 555)		\
	x(BCH_ERR_data_read_retry,	data_read_ptr_stale_retry, 556)		\
	x(BCH_ERR_data_read_retry_avoid,data_read_ptr_stale_dirty, 557)		\
	x(BCH_ERR_data_read,		data_read_no_encryption_key, 558)	\
	x(BCH_ERR_data_read,		data_read_buffer_too_small, 559)	\
	x(BCH_ERR_data_read,		data_read_key_overwritten, 560)		\
	x(0,				rbio_narrow_crcs_fail, 561)		\
	x(0,				nopromote, 562)				\
	x(BCH_ERR_nopromote,		nopromote_no_rewrites, 563)		\
	x(BCH_ERR_nopromote,		nopromote_already_promoted, 564)	\
	x(BCH_ERR_nopromote,		nopromote_unwritten, 565)		\
	x(BCH_ERR_nopromote,		nopromote_congested, 566)		\
	x(BCH_ERR_nopromote,		nopromote_ratelimited, 567)		\
	x(BCH_ERR_nopromote,		nopromote_no_writes, 568)		\
	x(BCH_ERR_nopromote,		nopromote_enomem, 569)			\
	x(0,				snapshot, 570)			\
	x(BCH_ERR_snapshot,		invalid_snapshot_node, 571)		\
	x(BCH_ERR_snapshot,		snapshot_multiple_descendents, 572)	\
	x(BCH_ERR_snapshot,		snapshot_lostfound_unreachable, 573)	\
	x(0,				option_needs_open_fs, 574)		\
	x(0,				remove_disk_accounting_entry, 575)	\
	x(0,				nocow_trylock_fail, 576)		\
	x(BCH_ERR_nocow_trylock_fail,	nocow_trylock_contended, 577)		\
	x(BCH_ERR_nocow_trylock_fail,	nocow_trylock_bucket_full, 578)		\
	x(EINTR,			cancelled, 579)				\
	x(BCH_ERR_cancelled,		recovery_cancelled, 580)		\
	x(BCH_ERR_cancelled,		kthread_cancelled, 581)			\
	x(BCH_ERR_cancelled,		snapshot_delete_cancelled, 582)		\
	x(0,				shutdown_with_errors, 583)		\
	x(BCH_ERR_shutdown_with_errors,	shutdown_with_errors_fixed, 584)	\
	x(BCH_ERR_shutdown_with_errors,	shutdown_with_errors_unfixed, 585)	\
	x(BCH_ERR_shutdown_with_errors,	shutdown_with_emergency_ro, 586)	\
	x(BCH_ERR_insufficient_devices_to_start,				\
					insufficient_devices_data_intact, 587)	\
	x(BCH_ERR_insufficient_devices_to_start,				\
					insufficient_devices_data_lost, 588)	\
	x(EIO,				injected_logged_op_fail, 589)		\
	x(EINVAL,			EINVAL_crypt_no_kdf_params, 590)	\
	x(EIO,				crypt_kdf_failed, 591)			\
	x(0,				reconcile_scan_stop, 592)		\
	x(0,				str_hash_key_repaired, 593)		\
	x(0,				delete_range_done, 594)			\
	x(0,				extent_iters_max, 595)

enum bch_errcode {
	BCH_ERR_START		= 2048,
#define x(class, err, nr) BCH_ERR_##err = BCH_ERR_START + nr,
	BCH_ERRCODES()
#undef x
	BCH_ERR_MAX
};

__attribute__((const)) const char *bch2_err_str(int);

__attribute__((const)) bool __bch2_err_matches(int, int);

__attribute__((const))
static inline bool _bch2_err_matches(int err, int class)
{
	return err < 0 && __bch2_err_matches(err, class);
}

#define bch2_err_matches(_err, _class)			\
({							\
	BUILD_BUG_ON(!__builtin_constant_p(_class));	\
	unlikely(_bch2_err_matches(_err, _class));	\
})

int __bch2_err_class(int);

static inline s64 bch2_err_class(s64 err)
{
	return err < 0 ? __bch2_err_class(err) : err;
}

#include <linux/blk_types.h>
const char *bch2_blk_status_to_str(blk_status_t);
enum bch_errcode blk_status_to_bch_err(blk_status_t);

#include <linux/zstd_errors.h>

enum bch_errcode zstd_err_to_bch_err(ZSTD_ErrorCode);

#endif /* _BCACHFES_ERRCODE_H */
