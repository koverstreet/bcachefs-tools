/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_OPTS_TYPES_H
#define _BCACHEFS_OPTS_TYPES_H

#include "enum_kind.h"

struct bch_fs;

/*
 * Mount options; we also store defaults in the superblock.
 *
 * Also exposed via sysfs: if an option is writeable, and it's also stored in
 * the superblock, changing it via sysfs (currently? might change this) also
 * updates the superblock.
 *
 * We store options as signed integers, where -1 means undefined. This means we
 * can pass the mount options to bch2_fs_alloc() as a whole struct, and then only
 * apply the options from that struct that are defined.
 */

/* When can be set: */
enum __enum_flags opt_flags {
	OPT_FS			= BIT(0),	/* Filesystem option */
	OPT_DEVICE		= BIT(1),	/* Device option */
	OPT_INODE		= BIT(2),	/* Inode option */
	OPT_FORMAT		= BIT(3),	/* May be specified at format time */
	OPT_MOUNT		= BIT(4),	/* May be specified at mount time */
	OPT_RUNTIME		= BIT(5),	/* May be specified at runtime */
	OPT_HUMAN_READABLE	= BIT(6),
	OPT_MUST_BE_POW_2	= BIT(7),	/* Must be power of 2 */
	OPT_SB_FIELD_SECTORS	= BIT(8),	/* Superblock field is >> 9 of actual value */
	OPT_SB_FIELD_ILOG2	= BIT(9),	/* Superblock field is ilog2 of actual value */
	OPT_SB_FIELD_ONE_BIAS	= BIT(10),	/* 0 means default value */
	OPT_HIDDEN		= BIT(11),
	OPT_MOUNT_OLD		= BIT(12),	/* May not be specified at mount time, but don't fail the mount */
	OPT_NODOC		= BIT(13),	/* Omit from generated documentation */
};

enum __enum_closed opt_type {
	BCH_OPT_BOOL,
	BCH_OPT_UINT,
	BCH_OPT_STR,
	BCH_OPT_BITFIELD,
	BCH_OPT_FN,
	/* A free-form string stored directly in a bch_member char[] field: */
	BCH_OPT_STR_MEMBER,
};

struct bch_opt_fn {
	int (*parse)(struct bch_fs *, const char *, u64 *, struct printbuf *);
	void (*to_text)(struct printbuf *, struct bch_fs *, struct bch_sb *, u64);
	int (*validate)(u64, struct printbuf *);
};

#ifdef __KERNEL__

#else

#define RATELIMIT_ERRORS_DEFAULT false

#endif

#ifdef CONFIG_BCACHEFS_DEBUG

#else

#define BCACHEFS_VERBOSE_DEFAULT	false

#endif

#define BCH_FIX_ERRORS_OPTS()		\
	x(exit,	0)			\
	x(yes,	1)			\
	x(no,	2)			\
	x(ask,	3)

enum __enum_closed fsck_err_opts {
#define x(t, n)	FSCK_FIX_##t,
	BCH_FIX_ERRORS_OPTS()
#undef x
};

#define BCH_OPTS()							\
	x(block_size,			u16,				\
	  OPT_FS|OPT_FORMAT|						\
	  OPT_HUMAN_READABLE|OPT_MUST_BE_POW_2|OPT_SB_FIELD_SECTORS,	\
	  OPT_UINT(512, 1U << 15),					\
	  BCH_SB_BLOCK_SIZE,		4 << 10,			\
	  "size",	"Filesystem block size")			\
	x(btree_node_size,		u32,				\
	  OPT_FS|OPT_FORMAT|						\
	  OPT_HUMAN_READABLE|OPT_MUST_BE_POW_2|OPT_SB_FIELD_SECTORS,	\
	  OPT_UINT(512, 1U << 19),					\
	  BCH_SB_BTREE_NODE_SIZE,	256 << 10,			\
	  "size",	"Btree node size, default 256k")		\
	x(errors,			u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME,			\
	  OPT_STR(bch2_error_actions),					\
	  BCH_SB_ERROR_ACTION,		BCH_ON_ERROR_fix_safe,		\
	  NULL,		"Action to take on filesystem error")		\
	x(write_error_timeout,		u16,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME,			\
	  OPT_UINT(1, 300),						\
	  BCH_SB_WRITE_ERROR_TIMEOUT,	30,				\
	  NULL,		"Number of consecutive write errors allowed before kicking out a device")\
	x(metadata_replicas,		u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,			\
	  OPT_UINT(1, BCH_REPLICAS_MAX),				\
	  BCH_SB_META_REPLICAS_WANT,	1,				\
	  "#",		"Number of metadata replicas (journal and btree)")\
	x(data_replicas,		u8,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,	\
	  OPT_UINT(1, BCH_REPLICAS_MAX),				\
	  BCH_SB_DATA_REPLICAS_WANT,	1,				\
	  "#",		"Number of data replicas (erasure coding currently caps this at 3, RAID6)")\
	x(encoded_extent_max,		u32,				\
	  OPT_FS|OPT_FORMAT|						\
	  OPT_HUMAN_READABLE|OPT_MUST_BE_POW_2|OPT_SB_FIELD_SECTORS|OPT_SB_FIELD_ILOG2,\
	  OPT_UINT(4096, 2U << 20),					\
	  BCH_SB_ENCODED_EXTENT_MAX_BITS, 256 << 10,			\
	  "size",	"Maximum size of checksummed/compressed extents")\
	x(metadata_checksum,		u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,			\
	  OPT_STR(__bch2_csum_opts),					\
	  BCH_SB_META_CSUM_TYPE,	BCH_CSUM_OPT_crc32c,		\
	  NULL,		"Checksum type for metadata writes")		\
	x(data_checksum,		u8,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,	\
	  OPT_STR(__bch2_csum_opts),					\
	  BCH_SB_DATA_CSUM_TYPE,	BCH_CSUM_OPT_crc32c,		\
	  NULL,		"Checksum type for data writes")		\
	x(checksum_err_retry_nr,	u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME,			\
	  OPT_UINT(0, 32),						\
	  BCH_SB_CSUM_ERR_RETRY_NR,	3,				\
	  NULL,		"Number of read retries on checksum error")	\
	x(compression,			u8,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,	\
	  OPT_FN(bch2_opt_compression),					\
	  BCH_SB_COMPRESSION_TYPE,	BCH_COMPRESSION_OPT_none,	\
	  NULL,		"Compression type for data writes")		\
	x(background_compression,	u8,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,	\
	  OPT_FN(bch2_opt_compression),					\
	  BCH_SB_BACKGROUND_COMPRESSION_TYPE,BCH_COMPRESSION_OPT_none,	\
	  NULL,		"Compression type for background moves")	\
	x(str_hash,			u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME,			\
	  OPT_STR(bch2_str_hash_opts),					\
	  BCH_SB_STR_HASH_TYPE,		BCH_STR_HASH_OPT_siphash,	\
	  NULL,		"Hash function for directory entries and xattrs")\
	x(metadata_target,		u16,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,			\
	  OPT_FN(bch2_opt_target),					\
	  BCH_SB_METADATA_TARGET,	0,				\
	  "(target)",	"Device or label for metadata writes")		\
	x(foreground_target,		u16,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,	\
	  OPT_FN(bch2_opt_target),					\
	  BCH_SB_FOREGROUND_TARGET,	0,				\
	  "(target)",	"Device or label for foreground writes")	\
	x(background_target,		u16,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,	\
	  OPT_FN(bch2_opt_target),					\
	  BCH_SB_BACKGROUND_TARGET,	0,				\
	  "(target)",	"Device or label to move data to in the background")\
	x(promote_target,		u16,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,	\
	  OPT_FN(bch2_opt_target),					\
	  BCH_SB_PROMOTE_TARGET,	0,				\
	  "(target)",	"Device or label to promote data to on read")	\
	x(erasure_code,			u16,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT_OLD|OPT_RUNTIME,	\
	  OPT_BOOL(),							\
	  BCH_SB_ERASURE_CODE,		false,				\
	  NULL,		"Enable erasure coding (RAID5/6; data replicas are capped at 3)")\
	x(ec_max_data_blocks,		u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME,			\
	  OPT_UINT(0, 15),						\
	  BCH_SB_EC_MAX_DATA_BLOCKS,	0,				\
	  NULL,		"Cap data blocks per EC stripe (0 = use all active devs)")\
	x(casefold,			u8,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT,					\
	  OPT_BOOL(),							\
	  BCH_SB_CASEFOLD,		false,				\
	  NULL,		"Dirent lookups are casefolded")		\
	x(casefold_disabled,			u8,			\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Disable casefolding filesystem wide")		\
	x(inodes_32bit,			u8,				\
	  OPT_FS|OPT_INODE|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME,		\
	  OPT_BOOL(),							\
	  BCH_SB_INODE_32BIT,		false,				\
	  NULL,		"Constrain inode numbers to 32 bits")		\
	x(shard_inode_numbers_bits,	u8,				\
	  OPT_FS|OPT_FORMAT,						\
	  OPT_UINT(0, 16),						\
	  BCH_SB_SHARD_INUMS_NBITS,	0,				\
	  NULL,		"Shard new inode numbers by CPU id")		\
	x(gc_reserve_percent,		u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME,			\
	  OPT_UINT(5, 20),						\
	  BCH_SB_GC_RESERVE,		8,				\
	  "%",		"Percentage of disk space to reserve for copygc")\
	x(gc_reserve_bytes,		u64,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME|			\
	  OPT_HUMAN_READABLE|OPT_SB_FIELD_SECTORS,			\
	  OPT_UINT(0, U64_MAX),						\
	  BCH_SB_GC_RESERVE_BYTES,	0,				\
	  "%",		"Amount of disk space to reserve for copygc\n"	\
			"Takes precedence over gc_reserve_percent if set")\
	x(root_reserve_percent,		u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT,					\
	  OPT_UINT(0, 100),						\
	  BCH_SB_ROOT_RESERVE,		0,				\
	  "%",		"Percentage of disk space to reserve for superuser")\
	x(wide_macs,			u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME,			\
	  OPT_BOOL(),							\
	  BCH_SB_128_BIT_MACS,		false,				\
	  NULL,		"Store full 128 bits of cryptographic MACs, instead of 80")\
	x(inline_data,			u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		true,				\
	  NULL,		"Enable inline data extents")			\
	x(promote_whole_extents,	u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH_SB_PROMOTE_WHOLE_EXTENTS,	true,				\
	  NULL,		"Promote whole extents, instead of just part being read")\
	x(acl,				u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT,					\
	  OPT_BOOL(),							\
	  BCH_SB_POSIX_ACL,		true,				\
	  NULL,		"Enable POSIX acls")				\
	x(usrquota,			u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT,					\
	  OPT_BOOL(),							\
	  BCH_SB_USRQUOTA,		false,				\
	  NULL,		"Enable user quotas")				\
	x(grpquota,			u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT,					\
	  OPT_BOOL(),							\
	  BCH_SB_GRPQUOTA,		false,				\
	  NULL,		"Enable group quotas")				\
	x(prjquota,			u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT,					\
	  OPT_BOOL(),							\
	  BCH_SB_PRJQUOTA,		false,				\
	  NULL,		"Enable project quotas")			\
	x(degraded,			u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_STR(bch2_degraded_actions),				\
	  BCH_SB_DEGRADED_ACTION,	BCH_DEGRADED_ask,		\
	  NULL,		"Allow mounting in degraded mode")		\
	x(write_degraded,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_FORMAT|OPT_RUNTIME,			\
	  OPT_STR(bch2_write_degraded_actions),				\
	  BCH_SB_WRITE_DEGRADED_ACTION,	BCH_WRITE_DEGRADED_degraded,	\
	  NULL,		"Write fewer copies than asked for rather than "	\
			"returning ENOSPC; the default does so only while "\
			"a device is missing or not read-write")	\
	x(missing_dev_timeout,		u32,				\
	  OPT_FS|OPT_MOUNT|OPT_FORMAT|OPT_RUNTIME,			\
	  OPT_UINT(0, 3600),						\
	  BCH_SB_EXT_MISSING_DEV_TIMEOUT, 0,				\
	  NULL,		"Seconds to wait at mount for member devices that "\
			"haven't appeared yet, before applying the degraded "\
			"action; 0 means use the built-in default")	\
	x(mount_trusts_udev,		u8,				\
	  OPT_MOUNT,							\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		true,				\
	  NULL,		"Trust udev when scanning for member devices")	\
	x(no_splitbrain_check,		u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Don't kick drives out when splitbrain detected")\
	x(no_version_check,		u8,				\
	  OPT_HIDDEN,							\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Don't fail reading the superblock due to incompatible version")\
	x(verbose,			u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		BCACHEFS_VERBOSE_DEFAULT,	\
	  NULL,		"Extra debugging information during mount/recovery")\
	x(journal_flush_delay,		u32,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_UINT(1, U32_MAX),						\
	  BCH_SB_JOURNAL_FLUSH_DELAY,	1000,				\
	  NULL,		"Delay in milliseconds before automatic journal commits")\
	x(journal_flush_disabled,	u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH_SB_JOURNAL_FLUSH_DISABLED,false,				\
	  NULL,		"Disable journal flush on sync/fsync\n"		\
			"If enabled, writes can be lost, but only since the\n"\
			"last journal write (default 1 second)")	\
	x(move_writes_fua,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH_SB_MOVE_WRITES_FUA,	false,				\
	  NULL,		"Issue writes from background data moves (copygc,\n"\
			"rebalance) with FUA, making them durable on\n"	\
			"completion rather than relying on the journal's\n"\
			"periodic cache flush")				\
	x(journal_reclaim_delay,	u32,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_UINT(0, U32_MAX),						\
	  BCH_SB_JOURNAL_RECLAIM_DELAY,	100,				\
	  NULL,		"Delay in milliseconds before automatic journal reclaim")\
	x(writeback_timeout,		u16,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_UINT(0, U16_MAX),						\
	  BCH_SB_WRITEBACK_TIMEOUT,	0,				\
	  NULL,		"Delay seconds before writing back dirty data, overriding vm sysctls")\
	x(move_bytes_in_flight,		u32,				\
	  OPT_HUMAN_READABLE|OPT_FS|OPT_MOUNT|OPT_RUNTIME|OPT_NODOC,	\
	  OPT_UINT(1024, U32_MAX),					\
	  BCH2_NO_SB_OPT,		64U << 20,			\
	  NULL,		"Maximum Amount of IO to keep in flight by the move path")\
	x(move_ios_in_flight,		u32,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME|OPT_NODOC,			\
	  OPT_UINT(1, 1024),						\
	  BCH2_NO_SB_OPT,		64,				\
	  NULL,		"Maximum number of IOs to keep in flight by the move path")\
	x(fsck,				u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Run fsck on mount")				\
	x(fsck_memory_usage_percent,	u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_UINT(20, 70),						\
	  BCH2_NO_SB_OPT,		50,				\
	  NULL,		"Maximum percentage of system ram fsck is allowed to pin")\
	x(fix_errors,			u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_FN(bch2_opt_fix_errors),					\
	  BCH2_NO_SB_OPT,		FSCK_FIX_exit,			\
	  NULL,		"Fix errors during fsck without asking")	\
	x(ratelimit_errors,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		RATELIMIT_ERRORS_DEFAULT,	\
	  NULL,		"Ratelimit error messages during fsck")		\
	x(no_commit_validate,		u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Disable commit-time-only bkey validation;\n"\
			"for error injection tools, which must be able\n"\
			"to write the states fsck is tested against")	\
	x(nochanges,			u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Super read only mode - no writes at all will be issued,\n"\
			"even if we have to replay the journal")	\
	x(norecovery,			u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Exit recovery immediately prior to journal replay")\
	x(journal_rewind,		u64,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_UINT(0, U64_MAX),						\
	  BCH2_NO_SB_OPT,		0,				\
	  NULL,		"Rewind journal")				\
	x(journal_rewind_discard_buffer_percent, u8,			\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_UINT(0, 10),						\
	  BCH_SB_EXT_DISCARD_BUFFER,	4,				\
	  NULL,		"Percentage of filesystem capacity to leave undiscarded"\
	  " for journal rewind")					\
	x(scrub_recent_journal_entries,	u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_STR(bch2_scrub_journal_opts),				\
	  BCH_SB_SCRUB_JOURNAL,		0,				\
	  NULL,		"Scrub data written in the last few journal entries during recovery")\
	x(scrub_journal_max_rewind_secs,	u32,			\
	  OPT_FS|OPT_MOUNT|OPT_FORMAT,					\
	  OPT_UINT(0, 3600),						\
	  BCH_SB_EXT_SCRUB_MAX_REWIND_SECS, 10,				\
	  NULL,		"Maximum time in seconds the journal scrub will rewind (default 10)")\
	x(recovery_passes,		u64,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BITFIELD(bch2_recovery_passes),				\
	  BCH2_NO_SB_OPT,		0,				\
	  NULL,		"Recovery passes to run explicitly")		\
	x(recovery_passes_exclude,	u64,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BITFIELD(bch2_recovery_passes),				\
	  BCH2_NO_SB_OPT,		0,				\
	  NULL,		"Recovery passes to exclude")			\
	x(recovery_pass_last,		u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_STR_NOLIMIT(bch2_recovery_passes),			\
	  BCH2_NO_SB_OPT,		0,				\
	  NULL,		"Exit recovery after specified pass")		\
	x(recovery_passes_skip_scheduled, u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Don't run repair passes, whether the superblock had "\
			"them scheduled or something schedules them mid-mount; "\
			"they stay scheduled for the next mount")	\
	x(retain_recovery_info,		u8,				\
	  0,								\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Don't free journal entries/keys, scanned btree nodes after startup")\
	x(read_entire_journal,		u8,				\
	  0,								\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Read all journal entries, not just dirty ones")\
	x(read_journal_only,		u8,				\
	  0,								\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Only read the journal, skip the rest of recovery")\
	x(journal_transaction_names,	u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME|OPT_NODOC,		\
	  OPT_BOOL(),							\
	  BCH_SB_JOURNAL_TRANSACTION_NAMES, true,			\
	  NULL,		"Log transaction function names in journal")	\
	x(allocator_stuck_timeout,	u16,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME|OPT_NODOC,		\
	  OPT_UINT(0, U16_MAX),						\
	  BCH_SB_ALLOCATOR_STUCK_TIMEOUT, 30,				\
	  NULL,		"Default timeout in seconds for stuck allocator messages")\
	x(noexcl,			u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Don't open device in exclusive mode")		\
	x(direct_io,			u8,				\
	  OPT_FS|OPT_MOUNT|OPT_NODOC,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,			true,			\
	  NULL,		"Use O_DIRECT (userspace only)")		\
	x(sb,				u64,				\
	  OPT_MOUNT,							\
	  OPT_UINT(0, S64_MAX),						\
	  BCH2_NO_SB_OPT,		BCH_SB_SECTOR,			\
	  "offset",	"Sector offset of superblock")			\
	x(read_only,			u8,				\
	  OPT_FS|OPT_MOUNT|OPT_HIDDEN,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		NULL)						\
	x(nostart,			u8,				\
	  0,								\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Don\'t start filesystem, only open devices")	\
	x(will_not_start,		u8,				\
	  OPT_HIDDEN,							\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		NULL)						\
	x(dangerously_reconstruct_alloc,u8,				\
	  OPT_FS|OPT_MOUNT,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Reconstruct alloc btree")			\
	x(version_upgrade,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_STR(bch2_version_upgrade_opts),				\
	  BCH_SB_VERSION_UPGRADE,	BCH_VERSION_UPGRADE_compatible,	\
	  NULL,		"Set superblock to latest version,\n"		\
			"allowing any new features to be used")		\
	x(stdio,			u64,				\
	  0,								\
	  OPT_UINT(0, S64_MAX),						\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Pointer to a struct stdio_redirect")		\
	x(project,			u8,				\
	  OPT_INODE|OPT_NODOC,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		NULL)						\
	x(nocow,			u8,				\
	  OPT_FS|OPT_FORMAT|OPT_MOUNT|OPT_RUNTIME|OPT_INODE,		\
	  OPT_BOOL(),							\
	  BCH_SB_NOCOW,			false,				\
	  NULL,		"Nocow mode: Writes will be done in place when possible.\n"\
			"Snapshots and reflink will still cause writes to be COW\n"\
			"Implicitly disables data checksumming, compression and encryption")\
	x(nocow_enabled,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_NODOC,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,			true,			\
	  NULL,		"Enable nocow mode: enables runtime locking in\n"\
			"data move path needed if nocow will ever be in use\n")\
	x(copygc_enabled,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,			true,			\
	  NULL,		"Enable copygc: disable for debugging, or to\n"\
			"quiet the system when doing performance testing\n")\
	x(reconcile_enabled,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,			true,			\
	  NULL,		"Enable reconcile: disable for debugging, or to\n"\
			"quiet the system when doing performance testing\n")\
	x(reconcile_on_ac_only,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH_SB_REBALANCE_AC_ONLY,		false,			\
	  NULL,		"Enable reconcile while on mains power only\n")	\
	x(auto_snapshot_deletion,	u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,			true,			\
	  NULL,		"Enable automatic snapshot deletion: disable for debugging, or to\n"\
			"quiet the system when doing performance testing\n")\
	x(no_data_io,			u8,				\
	  OPT_MOUNT|OPT_NODOC,						\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		false,				\
	  NULL,		"Skip submit_bio() for data reads and writes, "	\
			"for performance testing purposes")		\
	x(state,			u64,				\
	  OPT_DEVICE|OPT_RUNTIME,					\
	  OPT_STR(bch2_member_states),					\
	  BCH_MEMBER_STATE,		BCH_MEMBER_STATE_rw,		\
	  "state",	"rw,ro,failed,spare")				\
	x(label,			u16,				\
	  OPT_DEVICE|OPT_FORMAT|OPT_RUNTIME,				\
	  OPT_FN(bch2_opt_disk_label),					\
	  BCH_MEMBER_GROUP,		0,				\
	  "(label)",	"Device label: position in the label tree")	\
	x(failure_domain,		u8,				\
	  OPT_DEVICE|OPT_FORMAT|OPT_RUNTIME,				\
	  OPT_STR_MEMBER(failure_domain),				\
	  BCH2_NO_SB_OPT,		0,				\
	  "(domain)",	"Failure domain: devices sharing a name are in the\n"\
			"same failure domain; allocation spreads replicas\n"\
			"across domains (a hard requirement for erasure\n"\
			"coded stripe blocks)")				\
	x(bucket_size,			u32,				\
	  OPT_DEVICE|OPT_HUMAN_READABLE|OPT_SB_FIELD_SECTORS,		\
	  OPT_UINT(0, S64_MAX),						\
	  BCH_MEMBER_BUCKET_SIZE,	0,				\
	  "size",	"Specifies the bucket size; must be greater than the btree node size")\
	x(durability,			u8,				\
	  OPT_DEVICE|OPT_RUNTIME|OPT_SB_FIELD_ONE_BIAS,			\
	  OPT_UINT(0, BCH_MEMBER_DURABILITY_MAX - 1),			\
	  BCH_MEMBER_DURABILITY,	1,				\
	  "n",		"Data written to this device will be considered\n"\
			"to have already been replicated n times")	\
	x(data_allowed,			u8,				\
	  OPT_DEVICE|OPT_FORMAT,					\
	  OPT_BITFIELD_MASK(__bch2_data_types,				\
		  BIT(BCH_DATA_journal)|BIT(BCH_DATA_btree)|BIT(BCH_DATA_user)),\
	  BCH_MEMBER_DATA_ALLOWED,	BIT(BCH_DATA_journal)|BIT(BCH_DATA_btree)|BIT(BCH_DATA_user),\
	  "types",	"Allowed data types for this device: journal, btree, and/or user")\
	x(discard,			u8,				\
	  OPT_MOUNT|OPT_FS|OPT_DEVICE|OPT_RUNTIME,			\
	  OPT_BOOL(),							\
	  BCH_MEMBER_DISCARD,		true,				\
	  NULL,		"Enable discard/TRIM support")			\
	x(rotational,			u8,				\
	  OPT_DEVICE|OPT_RUNTIME,					\
	  OPT_BOOL(),							\
	  BCH_MEMBER_ROTATIONAL,	false,				\
	  NULL,		"Disk is rotational; different behaviour for reconcile")\
	x(btree_node_prefetch,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME|OPT_NODOC,			\
	  OPT_BOOL(),							\
	  BCH2_NO_SB_OPT,		true,				\
	  NULL,		"BTREE_ITER_prefetch causes btree nodes to be\n"\
	  " prefetched sequentially")				\
	x(btree_cache_shrinker_seeks,	u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,					\
	  OPT_UINT(1, 100),						\
	  BCH_SB_EXT_BTREE_CACHE_SHRINKER_SEEKS,	2,		\
	  NULL,		"Shrinker cost of re-reading a btree node;\n"	\
	  " higher means the btree cache is evicted less\n"	\
	  " aggressively under memory pressure. Consider\n"	\
	  " raising on rotational storage, where re-reading\n"	\
	  " evicted nodes is expensive (pinned nodes use 4x)")	\
	x(dev_readahead,		u64,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME|OPT_HUMAN_READABLE|OPT_SB_FIELD_SECTORS,\
	  OPT_UINT(0, BCH_SB_EXT_DEV_READAHEAD_MAX << 9),		\
	  BCH_SB_EXT_DEV_READAHEAD,	SZ_2M,				\
	  "size",	"Per-device readahead window size; summed across\n"\
	  " all devices to set the filesystem readahead")		\
	x(ec_stripe_buf_limit,		u8,				\
	  OPT_FS|OPT_MOUNT|OPT_RUNTIME,				\
	  OPT_UINT(1, 25),						\
	  BCH_SB_EXT_EC_STRIPE_BUF_LIMIT,	5,			\
	  "%",		"Maximum percentage of total RAM for in-flight\n"\
	  " EC stripe buffers")

enum __enum_open bch_opt_id {
#define x(_name, ...)	Opt_##_name,
	BCH_OPTS()
#undef x
	bch2_opts_nr
};

struct bch_opts_mask {
	unsigned long	d[BITS_TO_LONGS(bch2_opts_nr)];
};

struct bch_opts {
#define x(_name, _bits, ...)	unsigned _name##_defined:1;
	BCH_OPTS()
#undef x

#define x(_name, _bits, ...)	_bits	_name;
	BCH_OPTS()
#undef x
};

struct bch_status_fd;

struct bch2_opts_parse {
	struct bch_opts opts;

	/* to save opts that can't be parsed before the FS is opened: */
	struct printbuf parse_later;

	/*
	 * The devices to mount, accumulated across however many "source"
	 * parameters we were handed - see bch2_fs_parse_param(). Owned here
	 * and freed by bch2_fs_context_free(), not by whoever consumes it.
	 */
	darray_const_str devs;

	/*
	 * The status channel, when the caller asked for one with "status_fd" -
	 * see bch2_fs_parse_param(). @status is freed from the file's .release
	 * method, so we hold a reference to the file of our own for as long as
	 * the filesystem might still print there, and drop it in
	 * bch2_fs_context_free().
	 */
	struct bch_status_fd	*status;
	struct file		*status_file;

	/*
	 * The passphrase-derived key, when the caller handed it to us with
	 * "user_key" rather than leaving it in a keyring - see
	 * bch2_fs_parse_param(). Zeroed by bch2_fs_context_free() however the
	 * mount went.
	 */
	struct bch_key		user_key;
	bool			user_key_set;
};

struct bch_fs;

struct printbuf;

struct bch_option {
	struct attribute	attr;
	enum opt_type		type;
	enum opt_flags		flags;
	u64			min, max;

	const char * const *choices;
	u64			choices_allowed_mask;

	struct bch_opt_fn	fn;

	const char		*hint;
	const char		*help;

	u64			(*get_sb)(const struct bch_sb *);
	void			(*set_sb)(struct bch_sb *, u64);

	u64			(*get_member)(const struct bch_member *);
	void			(*set_member)(struct bch_member *, u64);

	u64			(*get_ext)(const struct bch_sb_field_ext *);
	void			(*set_ext)(struct bch_sb_field_ext *, u64);

	/* BCH_OPT_STR_MEMBER: the bch_member char[] field the string lives in */
	unsigned		member_offset;
	unsigned		member_size;
};

struct bch_dev;

struct opt_change_scope;

/* inode opts: */

struct bch_inode_opts {
#define x(_name, _bits)	u##_bits _name;
	BCH_INODE_OPTS()
#undef x

#define x(_name, _bits)	u64 _name##_from_inode:1;
	BCH_INODE_OPTS()
#undef x

	u32 change_cookie;
};

#endif /* _BCACHEFS_OPTS_TYPES_H */
