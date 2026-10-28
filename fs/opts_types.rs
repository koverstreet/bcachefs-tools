// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::cstructs::c::BCH_INODE_OPTS;
use crate::types::{bits_to_longs, c_default};
use core::ffi::c_ulong;
use cstruct_macros::{CStruct, c_const, c_enum, c_xmacro, TypeInfo};

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
c_enum! {
    #[flags]
    pub enum opt_flags: u32 {
        OPT_FS = 1 << 0,                /* Filesystem option */
        OPT_DEVICE = 1 << 1,            /* Device option */
        OPT_INODE = 1 << 2,             /* Inode option */
        OPT_FORMAT = 1 << 3,            /* May be specified at format time */
        OPT_MOUNT = 1 << 4,             /* May be specified at mount time */
        OPT_RUNTIME = 1 << 5,           /* May be specified at runtime */
        OPT_HUMAN_READABLE = 1 << 6,
        OPT_MUST_BE_POW_2 = 1 << 7,     /* Must be power of 2 */
        OPT_SB_FIELD_SECTORS = 1 << 8,  /* Superblock field is >> 9 of actual value */
        OPT_SB_FIELD_ILOG2 = 1 << 9,    /* Superblock field is ilog2 of actual value */
        OPT_SB_FIELD_ONE_BIAS = 1 << 10, /* 0 means default value */
        OPT_HIDDEN = 1 << 11,
        OPT_MOUNT_OLD = 1 << 12,        /* May not be specified at mount time, but don't fail the mount */
        OPT_NODOC = 1 << 13,            /* Omit from generated documentation */
    }
}

c_const! {
    /* the ratelimit_errors option's default: on in the kernel */
    pub const RATELIMIT_ERRORS_DEFAULT: bool = cfg!(__KERNEL__);

    /* the verbose option's default: on in debug builds */
    pub const BCACHEFS_VERBOSE_DEFAULT: bool = cfg!(CONFIG_BCACHEFS_DEBUG);

    /* bch2_opt_to_text()'s flags */
    pub const OPT_SHOW_FULL_LIST: u32 = 1 << 0;
    pub const OPT_SHOW_MOUNT_STYLE: u32 = 1 << 1;
}

c_xmacro! {
    BCH_FIX_ERRORS_OPTS(x) {
        (exit, 0),
        (yes, 1),
        (no, 2),
        (ask, 3),
    }
}

c_enum! {
    #[closed]
    pub enum fsck_err_opts: u32 {
        BCH_FIX_ERRORS_OPTS(t, ..) => FSCK_FIX_ ## t,
    }
}

/*
 * The options: (name, type in bch_opts, flags, OPT_*() type, superblock
 * field, default, hint, help). C reads the first two; the rest are Rust,
 * opts.rs's OPTS, and resolve there - a string or bitfield option's choices
 * as the enum they are, the superblock field as its getter, struct::getter,
 * or _ for none.
 */
c_xmacro! {
    BCH_OPTS(x) {
        (block_size,                    u16,
         OPT_FS | OPT_FORMAT |
         OPT_HUMAN_READABLE | OPT_MUST_BE_POW_2 | OPT_SB_FIELD_SECTORS,
         OPT_UINT(512, 1 << 15),
         bch_sb::block_size,             4 << 10,
         "size",        "Filesystem block size"),
        (btree_node_size,               u32,
         OPT_FS | OPT_FORMAT |
         OPT_HUMAN_READABLE | OPT_MUST_BE_POW_2 | OPT_SB_FIELD_SECTORS,
         OPT_UINT(512, 1 << 19),
         bch_sb::btree_node_size,        256 << 10,
         "size",        "Btree node size, default 256k"),
        (errors,                        u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_STR(bch_error_actions),
         bch_sb::error_action,           BCH_ON_ERROR_fix_safe,
         NULL,          "Action to take on filesystem error"),
        (write_error_timeout,           u16,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(1, 300),
         bch_sb::write_error_timeout,    30,
         NULL,          "Number of consecutive write errors allowed before kicking out a device"),
        (metadata_replicas,             u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_UINT(1, BCH_REPLICAS_MAX),
         bch_sb::meta_replicas_want,     1,
         "#",           "Number of metadata replicas (journal and btree)"),
        (journal_replicas,              u8,
         OPT_FS | OPT_FORMAT | OPT_RUNTIME,
         OPT_UINT(0, BCH_REPLICAS_MAX),
         bch_sb_field_ext::journal_replicas,   0,
         "#",           "Number of journal replicas, if not metadata_replicas"),
        (data_replicas,                 u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_UINT(1, BCH_REPLICAS_MAX),
         bch_sb::data_replicas_want,     1,
         "#",           "Number of data replicas (erasure coding currently caps this at 3, RAID6)"),
        (encoded_extent_max,            u32,
         OPT_FS | OPT_FORMAT |
         OPT_HUMAN_READABLE | OPT_MUST_BE_POW_2 | OPT_SB_FIELD_SECTORS | OPT_SB_FIELD_ILOG2,
         OPT_UINT(4096, 2 << 20),
         bch_sb::encoded_extent_max_bits, 256 << 10,
         "size",        "Maximum size of checksummed/compressed extents"),
        (metadata_checksum,             u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_STR(bch_csum_opt),
         bch_sb::meta_csum_type,         BCH_CSUM_OPT_crc32c,
         NULL,          "Checksum type for metadata writes"),
        (data_checksum,                 u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_STR(bch_csum_opt),
         bch_sb::data_csum_type,         BCH_CSUM_OPT_crc32c,
         NULL,          "Checksum type for data writes"),
        (checksum_err_retry_nr,         u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(0, 32),
         bch_sb::csum_err_retry_nr,      3,
         NULL,          "Number of read retries on checksum error"),
        (compression,                   u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(compress::OPT),
         bch_sb::compression_type,       BCH_COMPRESSION_OPT_none,
         NULL,          "Compression type for data writes"),
        (background_compression,        u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(compress::OPT),
         bch_sb::background_compression_type, BCH_COMPRESSION_OPT_none,
         NULL,          "Compression type for background moves"),
        (str_hash,                      u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_STR(bch_str_hash_opts),
         bch_sb::str_hash_type,          BCH_STR_HASH_OPT_siphash,
         NULL,          "Hash function for directory entries and xattrs"),
        (metadata_target,               u16,
         OPT_FS | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(disk_groups::TARGET_OPT),
         bch_sb::metadata_target,        0,
         "(target)",    "Device or label for metadata writes"),
        (foreground_target,             u16,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(disk_groups::TARGET_OPT),
         bch_sb::foreground_target,      0,
         "(target)",    "Device or label for foreground writes"),
        (background_target,             u16,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(disk_groups::TARGET_OPT),
         bch_sb::background_target,      0,
         "(target)",    "Device or label to move data to in the background"),
        (promote_target,                u16,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(disk_groups::TARGET_OPT),
         bch_sb::promote_target,         0,
         "(target)",    "Device or label to promote data to on read"),
        (erasure_code,                  u16,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_BOOL(),
         bch_sb::erasure_code,           false,
         NULL,          "Enable erasure coding (RAID5/6; data replicas are capped at 3)"),
        (ec_max_data_blocks,            u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(0, 15),
         bch_sb::ec_max_data_blocks,     0,
         NULL,          "Cap data blocks per EC stripe (0 = use all active devs)"),
        (casefold,                      u8,
         OPT_FS | OPT_INODE | OPT_FORMAT,
         OPT_BOOL(),
         bch_sb::casefold,               false,
         NULL,          "Dirent lookups are casefolded"),
        (casefold_disabled,             u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Disable casefolding filesystem wide"),
        (inodes_32bit,                  u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         bch_sb::inode_32bit,            false,
         NULL,          "Constrain inode numbers to 32 bits"),
        (shard_inode_numbers_bits,      u8,
         OPT_FS | OPT_FORMAT,
         OPT_UINT(0, 8),
         bch_sb::shard_inums_nbits,      0,
         NULL,          "Shard new inode numbers by CPU id"),
        (gc_reserve_percent,            u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(5, 20),
         bch_sb::gc_reserve,             8,
         "%",           "Percentage of disk space to reserve for copygc"),
        (gc_reserve_bytes,              u64,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME |
         OPT_HUMAN_READABLE | OPT_SB_FIELD_SECTORS,
         OPT_UINT(0, u64::MAX),
         bch_sb::gc_reserve_bytes,       0,
         "%",           "Amount of disk space to reserve for copygc\n"
                        "Takes precedence over gc_reserve_percent if set"),
        (root_reserve_percent,          u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_UINT(0, 100),
         bch_sb::root_reserve,           0,
         "%",           "Percentage of disk space to reserve for superuser"),
        (wide_macs,                     u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         bch_sb::_128_bit_macs / set_128_bit_macs,           false,
         NULL,          "Store full 128 bits of cryptographic MACs, instead of 80"),
        (inline_data,                   u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         _,                true,
         NULL,          "Enable inline data extents"),
        (promote_whole_extents,         u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         bch_sb::promote_whole_extents,  true,
         NULL,          "Promote whole extents, instead of just part being read"),
        (acl,                           u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_BOOL(),
         bch_sb::posix_acl,              true,
         NULL,          "Enable POSIX acls"),
        (usrquota,                      u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_BOOL(),
         bch_sb::usrquota,               false,
         NULL,          "Enable user quotas"),
        (grpquota,                      u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_BOOL(),
         bch_sb::grpquota,               false,
         NULL,          "Enable group quotas"),
        (prjquota,                      u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_BOOL(),
         bch_sb::prjquota,               false,
         NULL,          "Enable project quotas"),
        (degraded,                      u8,
         OPT_FS | OPT_MOUNT,
         OPT_STR(bch_degraded_actions),
         bch_sb::degraded_action,        BCH_DEGRADED_ask,
         NULL,          "Allow mounting in degraded mode"),
        (write_degraded,                u8,
         OPT_FS | OPT_MOUNT | OPT_FORMAT | OPT_RUNTIME,
         OPT_STR(bch_write_degraded_actions),
         bch_sb::write_degraded_action,  BCH_WRITE_DEGRADED_degraded,
         NULL,          "Write fewer copies than asked for rather than "
                        "returning ENOSPC; the default does so only while "
                        "a device is missing or not read-write"),
        (missing_dev_timeout,           u32,
         OPT_FS | OPT_MOUNT | OPT_FORMAT | OPT_RUNTIME,
         OPT_UINT(0, 3600),
         bch_sb_field_ext::missing_dev_timeout, 0,
         NULL,          "Seconds to wait at mount for member devices that "
                        "haven't appeared yet, before applying the degraded "
                        "action; 0 means use the built-in default"),
        (mount_trusts_udev,             u8,
         OPT_MOUNT,
         OPT_BOOL(),
         _,                true,
         NULL,          "Trust udev when scanning for member devices"),
        (no_splitbrain_check,           u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Don't kick drives out when splitbrain detected"),
        (no_version_check,              u8,
         OPT_HIDDEN,
         OPT_BOOL(),
         _,                false,
         NULL,          "Don't fail reading the superblock due to incompatible version"),
        (verbose,                       u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         _,                BCACHEFS_VERBOSE_DEFAULT,
         NULL,          "Extra debugging information during mount/recovery"),
        (journal_flush_delay,           u32,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(1, u32::MAX),
         bch_sb::journal_flush_delay,    1000,
         NULL,          "Delay in milliseconds before automatic journal commits"),
        (journal_flush_disabled,        u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         bch_sb::journal_flush_disabled, false,
         NULL,          "Disable journal flush on sync/fsync\n"
                        "If enabled, writes can be lost, but only since the\n"
                        "last journal write (default 1 second)"),
        (move_writes_fua,               u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         bch_sb::move_writes_fua,        false,
         NULL,          "Issue writes from background data moves (copygc,\n"
                        "rebalance) with FUA, making them durable on\n"
                        "completion rather than relying on the journal's\n"
                        "periodic cache flush"),
        (journal_reclaim_delay,         u32,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(0, u32::MAX),
         bch_sb::journal_reclaim_delay,  100,
         NULL,          "Delay in milliseconds before automatic journal reclaim"),
        (writeback_timeout,             u16,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(0, u16::MAX),
         bch_sb::writeback_timeout,      0,
         NULL,          "Delay seconds before writing back dirty data, overriding vm sysctls"),
        (move_bytes_in_flight,          u32,
         OPT_HUMAN_READABLE | OPT_FS | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_UINT(1024, u32::MAX),
         _,                64 << 20,
         NULL,          "Maximum Amount of IO to keep in flight by the move path"),
        (move_ios_in_flight,            u32,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_UINT(1, 1024),
         _,                64,
         NULL,          "Maximum number of IOs to keep in flight by the move path"),
        (fsck,                          u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Run fsck on mount"),
        (fsck_memory_usage_percent,     u8,
         OPT_FS | OPT_MOUNT,
         OPT_UINT(20, 70),
         _,                50,
         NULL,          "Maximum percentage of system ram fsck is allowed to pin"),
        (fix_errors,                    u8,
         OPT_FS | OPT_MOUNT,
         OPT_FN(FIX_ERRORS),
         _,                FSCK_FIX_exit,
         NULL,          "Fix errors during fsck without asking"),
        (ratelimit_errors,              u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         _,                RATELIMIT_ERRORS_DEFAULT,
         NULL,          "Ratelimit error messages during fsck"),
        (no_commit_validate,            u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Disable commit-time-only bkey validation;\n"
                        "for error injection tools, which must be able\n"
                        "to write the states fsck is tested against"),
        (nochanges,                     u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Super read only mode - no writes at all will be issued,\n"
                        "even if we have to replay the journal"),
        (norecovery,                    u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Exit recovery immediately prior to journal replay"),
        (journal_rewind,                u64,
         OPT_FS | OPT_MOUNT,
         OPT_UINT(0, u64::MAX),
         _,                0,
         NULL,          "Rewind journal"),
        (journal_rewind_discard_buffer_percent, u8,
         OPT_FS | OPT_MOUNT,
         OPT_UINT(0, 10),
         bch_sb_field_ext::discard_buffer,     4,
         NULL,          "Percentage of filesystem capacity to leave undiscarded"
                        " for journal rewind"),
        (scrub_recent_journal_entries,  u8,
         OPT_FS | OPT_MOUNT,
         OPT_STR(bch_scrub_journal_opts),
         bch_sb::scrub_journal,          0,
         NULL,          "Scrub data written in the last few journal entries during recovery"),
        (scrub_journal_max_rewind_secs, u32,
         OPT_FS | OPT_MOUNT | OPT_FORMAT,
         OPT_UINT(0, 3600),
         bch_sb_field_ext::scrub_max_rewind_secs, 10,
         NULL,          "Maximum time in seconds the journal scrub will rewind (default 10)"),
        (recovery_passes,               u64,
         OPT_FS | OPT_MOUNT,
         OPT_BITFIELD(bch_recovery_pass),
         _,                0,
         NULL,          "Recovery passes to run explicitly"),
        (recovery_passes_exclude,       u64,
         OPT_FS | OPT_MOUNT,
         OPT_BITFIELD(bch_recovery_pass),
         _,                0,
         NULL,          "Recovery passes to exclude"),
        (recovery_pass_last,            u8,
         OPT_FS | OPT_MOUNT,
         OPT_STR_NOLIMIT(bch_recovery_pass),
         _,                0,
         NULL,          "Exit recovery after specified pass"),
        (recovery_passes_skip_scheduled, u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Don't run repair passes, whether the superblock had "
                        "them scheduled or something schedules them mid-mount; "
                        "they stay scheduled for the next mount"),
        (retain_recovery_info,          u8,
         0,
         OPT_BOOL(),
         _,                false,
         NULL,          "Don't free journal entries/keys, scanned btree nodes after startup"),
        (read_entire_journal,           u8,
         0,
         OPT_BOOL(),
         _,                false,
         NULL,          "Read all journal entries, not just dirty ones"),
        (read_journal_only,             u8,
         0,
         OPT_BOOL(),
         _,                false,
         NULL,          "Only read the journal, skip the rest of recovery"),
        (journal_transaction_names,     u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_BOOL(),
         bch_sb::journal_transaction_names, true,
         NULL,          "Log transaction function names in journal"),
        (allocator_stuck_timeout,       u16,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_UINT(0, u16::MAX),
         bch_sb::allocator_stuck_timeout, 30,
         NULL,          "Default timeout in seconds for stuck allocator messages"),
        (noexcl,                        u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Don't open device in exclusive mode"),
        (direct_io,                     u8,
         OPT_FS | OPT_MOUNT | OPT_NODOC,
         OPT_BOOL(),
         _,                true,
         NULL,          "Use O_DIRECT (userspace only)"),
        (sb,                            u64,
         OPT_MOUNT,
         OPT_UINT(0, i64::MAX),
         _,                BCH_SB_SECTOR,
         "offset",      "Sector offset of superblock"),
        (read_only,                     u8,
         OPT_FS | OPT_MOUNT | OPT_HIDDEN,
         OPT_BOOL(),
         _,                false,
         NULL,          NULL),
        (nostart,                       u8,
         0,
         OPT_BOOL(),
         _,                false,
         NULL,          "Don\'t start filesystem, only open devices"),
        (will_not_start,                u8,
         OPT_HIDDEN,
         OPT_BOOL(),
         _,                false,
         NULL,          NULL),
        (dangerously_reconstruct_alloc, u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         _,                false,
         NULL,          "Reconstruct alloc btree"),
        (version_upgrade,               u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_STR(bch_version_upgrade_opts),
         bch_sb::version_upgrade,        BCH_VERSION_UPGRADE_compatible,
         NULL,          "Set superblock to latest version,\n"
                        "allowing any new features to be used"),
        (stdio,                         u64,
         0,
         OPT_UINT(0, i64::MAX),
         _,                false,
         NULL,          "Pointer to a struct stdio_redirect"),
        (project,                       u8,
         OPT_INODE | OPT_NODOC,
         OPT_BOOL(),
         _,                false,
         NULL,          NULL),
        (nocow,                         u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME | OPT_INODE,
         OPT_BOOL(),
         bch_sb::nocow,                  false,
         NULL,          "Nocow mode: Writes will be done in place when possible.\n"
                        "Snapshots and reflink will still cause writes to be COW\n"
                        "Implicitly disables data checksumming, compression and encryption"),
        (nocow_enabled,                 u8,
         OPT_FS | OPT_MOUNT | OPT_NODOC,
         OPT_BOOL(),
         _,                true,
         NULL,          "Enable nocow mode: enables runtime locking in\n"
                        "data move path needed if nocow will ever be in use\n"),
        (copygc_enabled,                u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         _,                true,
         NULL,          "Enable copygc: disable for debugging, or to\n"
                        "quiet the system when doing performance testing\n"),
        (reconcile_enabled,             u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         _,                true,
         NULL,          "Enable reconcile: disable for debugging, or to\n"
                        "quiet the system when doing performance testing\n"),
        (reconcile_on_ac_only,          u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         bch_sb::rebalance_ac_only,      false,
         NULL,          "Enable reconcile while on mains power only\n"),
        (auto_snapshot_deletion,        u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         _,                true,
         NULL,          "Enable automatic snapshot deletion: disable for debugging, or to\n"
                        "quiet the system when doing performance testing\n"),
        (no_data_io,                    u8,
         OPT_MOUNT | OPT_NODOC,
         OPT_BOOL(),
         _,                false,
         NULL,          "Skip submit_bio() for data reads and writes, "
                        "for performance testing purposes"),
        (state,                         u64,
         OPT_DEVICE | OPT_RUNTIME,
         OPT_STR(bch_member_state),
         bch_member::state,              BCH_MEMBER_STATE_rw,
         NULL,          "Device state"),
        (label,                         u16,
         OPT_DEVICE | OPT_FORMAT | OPT_RUNTIME,
         OPT_FN(disk_groups::LABEL_OPT),
         bch_member::group,              0,
         "(label)",     "Device label: position in the label tree"),
        (failure_domain,                u8,
         OPT_DEVICE | OPT_FORMAT | OPT_RUNTIME,
         OPT_STR_MEMBER(failure_domain),
         _,                0,
         "(domain)",    "Failure domain: devices sharing a name are in the\n"
                        "same failure domain; allocation spreads replicas\n"
                        "across domains (a hard requirement for erasure\n"
                        "coded stripe blocks)"),
        (bucket_size,                   u32,
         OPT_DEVICE | OPT_HUMAN_READABLE | OPT_SB_FIELD_SECTORS,
         OPT_UINT(0, i64::MAX),
         bch_member::bucket_size,        0,
         "size",        "Specifies the bucket size; must be greater than the btree node size"),
        (durability,                    u8,
         OPT_DEVICE | OPT_RUNTIME | OPT_SB_FIELD_ONE_BIAS,
         OPT_UINT(0, bch_member::DURABILITY_MAX - 1),
         bch_member::durability,         1,
         "n",           "Data written to this device will be considered\n"
                        "to have already been replicated n times"),
        (data_allowed,                  u8,
         OPT_DEVICE | OPT_FORMAT,
         OPT_BITFIELD_MASK(bch_data_type,
                 1 << BCH_DATA_journal | 1 << BCH_DATA_btree | 1 << BCH_DATA_user),
         bch_member::data_allowed,       1 << BCH_DATA_journal | 1 << BCH_DATA_btree | 1 << BCH_DATA_user,
         "types",       "Allowed data types for this device: journal, btree, and/or user"),
        (discard,                       u8,
         OPT_MOUNT | OPT_FS | OPT_DEVICE | OPT_RUNTIME,
         OPT_BOOL(),
         bch_member::discard,            true,
         NULL,          "Enable discard/TRIM support"),
        (rotational,                    u8,
         OPT_DEVICE | OPT_RUNTIME,
         OPT_BOOL(),
         bch_member::rotational,         false,
         NULL,          "Disk is rotational; different behaviour for reconcile"),
        (btree_node_prefetch,           u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_BOOL(),
         _,                true,
         NULL,          "BTREE_ITER_prefetch causes btree nodes to be\n"
                        " prefetched sequentially"),
        (btree_cache_shrinker_seeks,    u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(1, 100),
         bch_sb_field_ext::btree_cache_shrinker_seeks, 2,
         NULL,          "Shrinker cost of re-reading a btree node;\n"
                        " higher means the btree cache is evicted less\n"
                        " aggressively under memory pressure. Consider\n"
                        " raising on rotational storage, where re-reading\n"
                        " evicted nodes is expensive (pinned nodes use 4x)"),
        (dev_readahead,                 u64,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME | OPT_HUMAN_READABLE | OPT_SB_FIELD_SECTORS,
         OPT_UINT(0, bch_sb_field_ext::DEV_READAHEAD_MAX << 9),
         bch_sb_field_ext::dev_readahead,      2 << 20,
         "size",        "Per-device readahead window size; summed across\n"
                        " all devices to set the filesystem readahead"),
        (ec_stripe_buf_limit,           u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(1, 25),
         bch_sb_field_ext::ec_stripe_buf_limit, 5,
         "%",           "Maximum percentage of total RAM for in-flight\n"
                        " EC stripe buffers"),
    }
}

c_enum! {
    #[open]
    pub enum bch_opt_id: u32 {
        BCH_OPTS(_name, ..) => Opt_ ## _name,
        bch2_opts_nr,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_opts_mask {
    #[c("unsigned long d[BITS_TO_LONGS(bch2_opts_nr)]")]
    pub d: [c_ulong; bits_to_longs(c::bch2_opts_nr as usize)],
}

macro_rules! __bch_opts_0 {
    ([$($acc:tt)*] $(($_name:tt $(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        const _: () = {
            #[allow(non_camel_case_types)]
            enum Bit { $($_name,)* }

            impl bch_opts {
                $(
                    pub fn [<$_name _defined>](&self) -> bool {
                        crate::types::c_bit(&self.__bch_opts_bits, Bit::$_name as usize)
                    }

                    pub fn [<set_ $_name _defined>](&mut self, v: bool) {
                        crate::types::set_c_bit(&mut self.__bch_opts_bits, Bit::$_name as usize, v)
                    }
                )*
            }
        };

        BCH_OPTS!(__bch_opts_1 [
            $($acc)*
            #[c_anon("#define x(_name, _bits, ...)	unsigned _name##_defined:1;
BCH_OPTS()
#undef x")]
            pub __bch_opts_bits: [u8; ([$(stringify!($_name)),*].len() * 1).div_ceil(8)],
        ]);
    } };
}
macro_rules! __bch_opts_1 {
    ([$($acc:tt)*] $(($_name:tt, $_bits:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        #[repr(C)]
        #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
        pub struct bch_opts {
            $($acc)*
            $(pub $_name: $_bits,)*
        }
    } };
}
BCH_OPTS!(__bch_opts_0 [#[c_anon("")] pub __bch_opts_align: [u32; 0],]);

#[repr(C)]
#[derive(CStruct)]
pub struct bch2_opts_parse {
    pub opts: c::bch_opts,

    /* to save opts that can't be parsed before the FS is opened: */
    pub parse_later: c::printbuf,

    /*
     * The devices to mount, accumulated across however many "source"
     * parameters we were handed - see bch2_fs_parse_param(). Owned here
     * and freed by bch2_fs_context_free(), not by whoever consumes it.
     */
    pub devs: c::darray_const_str,

    /*
     * The status channel, when the caller asked for one with "status_fd" -
     * see bch2_fs_parse_param() and vfs/status_fd.rs. The file's .release
     * frees the channel, so we hold a reference to the file of our own for
     * as long as the filesystem might still print there, and drop it in
     * bch2_fs_context_free().
     */
    pub status_file: *mut c::file,

    /*
     * The passphrase-derived key, when the caller handed it to us with
     * "user_key" rather than leaving it in a keyring - see
     * bch2_fs_parse_param(). Zeroed by bch2_fs_context_free() however the
     * mount went.
     */
    pub user_key: c::bch_key,
    pub user_key_set: bool,
}
c_default!(bch2_opts_parse);

/* inode opts: */
macro_rules! __bch_inode_opts_0 {
    ([$($acc:tt)*] $(($_name:tt, $_bits:tt)),* $(,)?) => { ::paste::paste! {
        BCH_INODE_OPTS!(__bch_inode_opts_1 [
            $($acc)*
            $(pub $_name: [<u $_bits>],)*
        ]);
    } };
}
macro_rules! __bch_inode_opts_1 {
    ([$($acc:tt)*] $(($_name:tt $(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        const _: () = {
            #[allow(non_camel_case_types)]
            enum Bit { $($_name,)* }

            impl bch_inode_opts {
                $(
                    pub fn [<$_name _from_inode>](&self) -> bool {
                        crate::types::c_bit(&self.__bch_inode_opts_bits, Bit::$_name as usize)
                    }

                    pub fn [<set_ $_name _from_inode>](&mut self, v: bool) {
                        crate::types::set_c_bit(&mut self.__bch_inode_opts_bits, Bit::$_name as usize, v)
                    }
                )*
            }
        };

        #[repr(C)]
        #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
        pub struct bch_inode_opts {
            $($acc)*
            #[c_anon("#define x(_name, _bits)	u64 _name##_from_inode:1;
BCH_INODE_OPTS()
#undef x")]
            pub __bch_inode_opts_bits: [u8; ([$(stringify!($_name)),*].len() * 1).div_ceil(8)],

            pub change_cookie: u32,
        }
    } };
}
BCH_INODE_OPTS!(__bch_inode_opts_0 [#[c_anon("")] pub __bch_inode_opts_align: [u64; 0],]);

