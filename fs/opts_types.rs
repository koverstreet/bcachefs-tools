// SPDX-License-Identifier: GPL-2.0

//! The data types of opts_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::cstructs::c::BCH_INODE_OPTS;
use crate::types::{bits_to_longs, c_opaque, c_default};
use cstruct_macros::{CStruct, c_const, c_enum, c_extern, c_verbatim, c_xmacro};
use typeinfo_macros::TypeInfo;

c_verbatim!(r#"
struct bch_fs;
"#);

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

c_enum! {
    #[closed]
    pub enum opt_type: u32 {
        BCH_OPT_BOOL,
        BCH_OPT_UINT,
        BCH_OPT_STR,
        BCH_OPT_BITFIELD,
        BCH_OPT_FN,
        /* A free-form string stored directly in a bch_member char[] field: */
        BCH_OPT_STR_MEMBER,
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_opt_fn {
    pub parse: Option<unsafe extern "C" fn(*mut c::bch_fs, *const crate::util::ffi::c_char, *mut u64, *mut c::printbuf) -> core::ffi::c_int>,
    pub to_text: Option<unsafe extern "C" fn(*mut c::printbuf, *mut c::bch_fs, *mut c::bch_sb, u64)>,
    pub validate: Option<unsafe extern "C" fn(u64, *mut c::printbuf) -> core::ffi::c_int>,
}
c_default!(bch_opt_fn);

c_const! {
    /* the ratelimit_errors option's default: on in the kernel */
    pub const RATELIMIT_ERRORS_DEFAULT: bool = cfg!(__KERNEL__);
}

c_const! {
    /* the verbose option's default: on in debug builds */
    pub const BCACHEFS_VERBOSE_DEFAULT: bool = cfg!(CONFIG_BCACHEFS_DEBUG);
}

c_xmacro! {
    BCH_FIX_ERRORS_OPTS(x) {
        (exit, 0),
        (yes, 1),
        (no, 2),
        (ask, 3),
    }
}

macro_rules! __fsck_err_opts_0 {
    ([$($acc:tt)*] $(($t:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum fsck_err_opts: u32 {
                $($acc)*
                $([<FSCK_FIX_ $t>],)*
            }
        }
    } };
}
BCH_FIX_ERRORS_OPTS!(__fsck_err_opts_0 []);

c_xmacro! {
    BCH_OPTS(x) {
        (block_size,                    u16,
         OPT_FS | OPT_FORMAT |
         OPT_HUMAN_READABLE | OPT_MUST_BE_POW_2 | OPT_SB_FIELD_SECTORS,
         OPT_UINT(512, 1U << 15),
         BCH_SB_BLOCK_SIZE,             4 << 10,
         "size",        "Filesystem block size"),
        (btree_node_size,               u32,
         OPT_FS | OPT_FORMAT |
         OPT_HUMAN_READABLE | OPT_MUST_BE_POW_2 | OPT_SB_FIELD_SECTORS,
         OPT_UINT(512, 1U << 19),
         BCH_SB_BTREE_NODE_SIZE,        256 << 10,
         "size",        "Btree node size, default 256k"),
        (errors,                        u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_STR(bch2_error_actions),
         BCH_SB_ERROR_ACTION,           BCH_ON_ERROR_fix_safe,
         NULL,          "Action to take on filesystem error"),
        (write_error_timeout,           u16,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(1, 300),
         BCH_SB_WRITE_ERROR_TIMEOUT,    30,
         NULL,          "Number of consecutive write errors allowed before kicking out a device"),
        (metadata_replicas,             u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_UINT(1, BCH_REPLICAS_MAX),
         BCH_SB_META_REPLICAS_WANT,     1,
         "#",           "Number of metadata replicas (journal and btree)"),
        (data_replicas,                 u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_UINT(1, BCH_REPLICAS_MAX),
         BCH_SB_DATA_REPLICAS_WANT,     1,
         "#",           "Number of data replicas (erasure coding currently caps this at 3, RAID6)"),
        (encoded_extent_max,            u32,
         OPT_FS | OPT_FORMAT |
         OPT_HUMAN_READABLE | OPT_MUST_BE_POW_2 | OPT_SB_FIELD_SECTORS | OPT_SB_FIELD_ILOG2,
         OPT_UINT(4096, 2U << 20),
         BCH_SB_ENCODED_EXTENT_MAX_BITS, 256 << 10,
         "size",        "Maximum size of checksummed/compressed extents"),
        (metadata_checksum,             u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_STR(__bch2_csum_opts),
         BCH_SB_META_CSUM_TYPE,         BCH_CSUM_OPT_crc32c,
         NULL,          "Checksum type for metadata writes"),
        (data_checksum,                 u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_STR(__bch2_csum_opts),
         BCH_SB_DATA_CSUM_TYPE,         BCH_CSUM_OPT_crc32c,
         NULL,          "Checksum type for data writes"),
        (checksum_err_retry_nr,         u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(0, 32),
         BCH_SB_CSUM_ERR_RETRY_NR,      3,
         NULL,          "Number of read retries on checksum error"),
        (compression,                   u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(bch2_opt_compression),
         BCH_SB_COMPRESSION_TYPE,       BCH_COMPRESSION_OPT_none,
         NULL,          "Compression type for data writes"),
        (background_compression,        u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(bch2_opt_compression),
         BCH_SB_BACKGROUND_COMPRESSION_TYPE, BCH_COMPRESSION_OPT_none,
         NULL,          "Compression type for background moves"),
        (str_hash,                      u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_STR(bch2_str_hash_opts),
         BCH_SB_STR_HASH_TYPE,          BCH_STR_HASH_OPT_siphash,
         NULL,          "Hash function for directory entries and xattrs"),
        (metadata_target,               u16,
         OPT_FS | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(bch2_opt_target),
         BCH_SB_METADATA_TARGET,        0,
         "(target)",    "Device or label for metadata writes"),
        (foreground_target,             u16,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(bch2_opt_target),
         BCH_SB_FOREGROUND_TARGET,      0,
         "(target)",    "Device or label for foreground writes"),
        (background_target,             u16,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(bch2_opt_target),
         BCH_SB_BACKGROUND_TARGET,      0,
         "(target)",    "Device or label to move data to in the background"),
        (promote_target,                u16,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_FN(bch2_opt_target),
         BCH_SB_PROMOTE_TARGET,         0,
         "(target)",    "Device or label to promote data to on read"),
        (erasure_code,                  u16,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT_OLD | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_SB_ERASURE_CODE,           false,
         NULL,          "Enable erasure coding (RAID5/6; data replicas are capped at 3)"),
        (ec_max_data_blocks,            u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(0, 15),
         BCH_SB_EC_MAX_DATA_BLOCKS,     0,
         NULL,          "Cap data blocks per EC stripe (0 = use all active devs)"),
        (casefold,                      u8,
         OPT_FS | OPT_INODE | OPT_FORMAT,
         OPT_BOOL(),
         BCH_SB_CASEFOLD,               false,
         NULL,          "Dirent lookups are casefolded"),
        (casefold_disabled,             u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Disable casefolding filesystem wide"),
        (inodes_32bit,                  u8,
         OPT_FS | OPT_INODE | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_SB_INODE_32BIT,            false,
         NULL,          "Constrain inode numbers to 32 bits"),
        (shard_inode_numbers_bits,      u8,
         OPT_FS | OPT_FORMAT,
         OPT_UINT(0, 16),
         BCH_SB_SHARD_INUMS_NBITS,      0,
         NULL,          "Shard new inode numbers by CPU id"),
        (gc_reserve_percent,            u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(5, 20),
         BCH_SB_GC_RESERVE,             8,
         "%",           "Percentage of disk space to reserve for copygc"),
        (gc_reserve_bytes,              u64,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME |
         OPT_HUMAN_READABLE | OPT_SB_FIELD_SECTORS,
         OPT_UINT(0, U64_MAX),
         BCH_SB_GC_RESERVE_BYTES,       0,
         "%",           "Amount of disk space to reserve for copygc\n"
                        "Takes precedence over gc_reserve_percent if set"),
        (root_reserve_percent,          u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_UINT(0, 100),
         BCH_SB_ROOT_RESERVE,           0,
         "%",           "Percentage of disk space to reserve for superuser"),
        (wide_macs,                     u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_SB_128_BIT_MACS,           false,
         NULL,          "Store full 128 bits of cryptographic MACs, instead of 80"),
        (inline_data,                   u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                true,
         NULL,          "Enable inline data extents"),
        (promote_whole_extents,         u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_SB_PROMOTE_WHOLE_EXTENTS,  true,
         NULL,          "Promote whole extents, instead of just part being read"),
        (acl,                           u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_BOOL(),
         BCH_SB_POSIX_ACL,              true,
         NULL,          "Enable POSIX acls"),
        (usrquota,                      u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_BOOL(),
         BCH_SB_USRQUOTA,               false,
         NULL,          "Enable user quotas"),
        (grpquota,                      u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_BOOL(),
         BCH_SB_GRPQUOTA,               false,
         NULL,          "Enable group quotas"),
        (prjquota,                      u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT,
         OPT_BOOL(),
         BCH_SB_PRJQUOTA,               false,
         NULL,          "Enable project quotas"),
        (degraded,                      u8,
         OPT_FS | OPT_MOUNT,
         OPT_STR(bch2_degraded_actions),
         BCH_SB_DEGRADED_ACTION,        BCH_DEGRADED_ask,
         NULL,          "Allow mounting in degraded mode"),
        (write_degraded,                u8,
         OPT_FS | OPT_MOUNT | OPT_FORMAT | OPT_RUNTIME,
         OPT_STR(bch2_write_degraded_actions),
         BCH_SB_WRITE_DEGRADED_ACTION,  BCH_WRITE_DEGRADED_degraded,
         NULL,          "Write fewer copies than asked for rather than "
                        "returning ENOSPC; the default does so only while "
                        "a device is missing or not read-write"),
        (missing_dev_timeout,           u32,
         OPT_FS | OPT_MOUNT | OPT_FORMAT | OPT_RUNTIME,
         OPT_UINT(0, 3600),
         BCH_SB_EXT_MISSING_DEV_TIMEOUT, 0,
         NULL,          "Seconds to wait at mount for member devices that "
                        "haven't appeared yet, before applying the degraded "
                        "action; 0 means use the built-in default"),
        (mount_trusts_udev,             u8,
         OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                true,
         NULL,          "Trust udev when scanning for member devices"),
        (no_splitbrain_check,           u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Don't kick drives out when splitbrain detected"),
        (no_version_check,              u8,
         OPT_HIDDEN,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Don't fail reading the superblock due to incompatible version"),
        (verbose,                       u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                BCACHEFS_VERBOSE_DEFAULT,
         NULL,          "Extra debugging information during mount/recovery"),
        (journal_flush_delay,           u32,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(1, U32_MAX),
         BCH_SB_JOURNAL_FLUSH_DELAY,    1000,
         NULL,          "Delay in milliseconds before automatic journal commits"),
        (journal_flush_disabled,        u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_SB_JOURNAL_FLUSH_DISABLED, false,
         NULL,          "Disable journal flush on sync/fsync\n"
                        "If enabled, writes can be lost, but only since the\n"
                        "last journal write (default 1 second)"),
        (move_writes_fua,               u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_SB_MOVE_WRITES_FUA,        false,
         NULL,          "Issue writes from background data moves (copygc,\n"
                        "rebalance) with FUA, making them durable on\n"
                        "completion rather than relying on the journal's\n"
                        "periodic cache flush"),
        (journal_reclaim_delay,         u32,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(0, U32_MAX),
         BCH_SB_JOURNAL_RECLAIM_DELAY,  100,
         NULL,          "Delay in milliseconds before automatic journal reclaim"),
        (writeback_timeout,             u16,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(0, U16_MAX),
         BCH_SB_WRITEBACK_TIMEOUT,      0,
         NULL,          "Delay seconds before writing back dirty data, overriding vm sysctls"),
        (move_bytes_in_flight,          u32,
         OPT_HUMAN_READABLE | OPT_FS | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_UINT(1024, U32_MAX),
         BCH2_NO_SB_OPT,                64U << 20,
         NULL,          "Maximum Amount of IO to keep in flight by the move path"),
        (move_ios_in_flight,            u32,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_UINT(1, 1024),
         BCH2_NO_SB_OPT,                64,
         NULL,          "Maximum number of IOs to keep in flight by the move path"),
        (fsck,                          u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Run fsck on mount"),
        (fsck_memory_usage_percent,     u8,
         OPT_FS | OPT_MOUNT,
         OPT_UINT(20, 70),
         BCH2_NO_SB_OPT,                50,
         NULL,          "Maximum percentage of system ram fsck is allowed to pin"),
        (fix_errors,                    u8,
         OPT_FS | OPT_MOUNT,
         OPT_FN(bch2_opt_fix_errors),
         BCH2_NO_SB_OPT,                FSCK_FIX_exit,
         NULL,          "Fix errors during fsck without asking"),
        (ratelimit_errors,              u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                RATELIMIT_ERRORS_DEFAULT,
         NULL,          "Ratelimit error messages during fsck"),
        (no_commit_validate,            u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Disable commit-time-only bkey validation;\n"
                        "for error injection tools, which must be able\n"
                        "to write the states fsck is tested against"),
        (nochanges,                     u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Super read only mode - no writes at all will be issued,\n"
                        "even if we have to replay the journal"),
        (norecovery,                    u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Exit recovery immediately prior to journal replay"),
        (journal_rewind,                u64,
         OPT_FS | OPT_MOUNT,
         OPT_UINT(0, U64_MAX),
         BCH2_NO_SB_OPT,                0,
         NULL,          "Rewind journal"),
        (journal_rewind_discard_buffer_percent, u8,
         OPT_FS | OPT_MOUNT,
         OPT_UINT(0, 10),
         BCH_SB_EXT_DISCARD_BUFFER,     4,
         NULL,          "Percentage of filesystem capacity to leave undiscarded"
                        " for journal rewind"),
        (scrub_recent_journal_entries,  u8,
         OPT_FS | OPT_MOUNT,
         OPT_STR(bch2_scrub_journal_opts),
         BCH_SB_SCRUB_JOURNAL,          0,
         NULL,          "Scrub data written in the last few journal entries during recovery"),
        (scrub_journal_max_rewind_secs, u32,
         OPT_FS | OPT_MOUNT | OPT_FORMAT,
         OPT_UINT(0, 3600),
         BCH_SB_EXT_SCRUB_MAX_REWIND_SECS, 10,
         NULL,          "Maximum time in seconds the journal scrub will rewind (default 10)"),
        (recovery_passes,               u64,
         OPT_FS | OPT_MOUNT,
         OPT_BITFIELD(bch2_recovery_passes),
         BCH2_NO_SB_OPT,                0,
         NULL,          "Recovery passes to run explicitly"),
        (recovery_passes_exclude,       u64,
         OPT_FS | OPT_MOUNT,
         OPT_BITFIELD(bch2_recovery_passes),
         BCH2_NO_SB_OPT,                0,
         NULL,          "Recovery passes to exclude"),
        (recovery_pass_last,            u8,
         OPT_FS | OPT_MOUNT,
         OPT_STR_NOLIMIT(bch2_recovery_passes),
         BCH2_NO_SB_OPT,                0,
         NULL,          "Exit recovery after specified pass"),
        (recovery_passes_skip_scheduled, u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Don't run repair passes, whether the superblock had "
                        "them scheduled or something schedules them mid-mount; "
                        "they stay scheduled for the next mount"),
        (retain_recovery_info,          u8,
         0,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Don't free journal entries/keys, scanned btree nodes after startup"),
        (read_entire_journal,           u8,
         0,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Read all journal entries, not just dirty ones"),
        (read_journal_only,             u8,
         0,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Only read the journal, skip the rest of recovery"),
        (journal_transaction_names,     u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_BOOL(),
         BCH_SB_JOURNAL_TRANSACTION_NAMES, true,
         NULL,          "Log transaction function names in journal"),
        (allocator_stuck_timeout,       u16,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_UINT(0, U16_MAX),
         BCH_SB_ALLOCATOR_STUCK_TIMEOUT, 30,
         NULL,          "Default timeout in seconds for stuck allocator messages"),
        (noexcl,                        u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Don't open device in exclusive mode"),
        (direct_io,                     u8,
         OPT_FS | OPT_MOUNT | OPT_NODOC,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                true,
         NULL,          "Use O_DIRECT (userspace only)"),
        (sb,                            u64,
         OPT_MOUNT,
         OPT_UINT(0, S64_MAX),
         BCH2_NO_SB_OPT,                BCH_SB_SECTOR,
         "offset",      "Sector offset of superblock"),
        (read_only,                     u8,
         OPT_FS | OPT_MOUNT | OPT_HIDDEN,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          NULL),
        (nostart,                       u8,
         0,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Don\'t start filesystem, only open devices"),
        (will_not_start,                u8,
         OPT_HIDDEN,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          NULL),
        (dangerously_reconstruct_alloc, u8,
         OPT_FS | OPT_MOUNT,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Reconstruct alloc btree"),
        (version_upgrade,               u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_STR(bch2_version_upgrade_opts),
         BCH_SB_VERSION_UPGRADE,        BCH_VERSION_UPGRADE_compatible,
         NULL,          "Set superblock to latest version,\n"
                        "allowing any new features to be used"),
        (stdio,                         u64,
         0,
         OPT_UINT(0, S64_MAX),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Pointer to a struct stdio_redirect"),
        (project,                       u8,
         OPT_INODE | OPT_NODOC,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          NULL),
        (nocow,                         u8,
         OPT_FS | OPT_FORMAT | OPT_MOUNT | OPT_RUNTIME | OPT_INODE,
         OPT_BOOL(),
         BCH_SB_NOCOW,                  false,
         NULL,          "Nocow mode: Writes will be done in place when possible.\n"
                        "Snapshots and reflink will still cause writes to be COW\n"
                        "Implicitly disables data checksumming, compression and encryption"),
        (nocow_enabled,                 u8,
         OPT_FS | OPT_MOUNT | OPT_NODOC,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                true,
         NULL,          "Enable nocow mode: enables runtime locking in\n"
                        "data move path needed if nocow will ever be in use\n"),
        (copygc_enabled,                u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                true,
         NULL,          "Enable copygc: disable for debugging, or to\n"
                        "quiet the system when doing performance testing\n"),
        (reconcile_enabled,             u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                true,
         NULL,          "Enable reconcile: disable for debugging, or to\n"
                        "quiet the system when doing performance testing\n"),
        (reconcile_on_ac_only,          u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_SB_REBALANCE_AC_ONLY,      false,
         NULL,          "Enable reconcile while on mains power only\n"),
        (auto_snapshot_deletion,        u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                true,
         NULL,          "Enable automatic snapshot deletion: disable for debugging, or to\n"
                        "quiet the system when doing performance testing\n"),
        (no_data_io,                    u8,
         OPT_MOUNT | OPT_NODOC,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                false,
         NULL,          "Skip submit_bio() for data reads and writes, "
                        "for performance testing purposes"),
        (state,                         u64,
         OPT_DEVICE | OPT_RUNTIME,
         OPT_STR(bch2_member_states),
         BCH_MEMBER_STATE,              BCH_MEMBER_STATE_rw,
         "state",       "rw,ro,failed,spare"),
        (label,                         u16,
         OPT_DEVICE | OPT_FORMAT | OPT_RUNTIME,
         OPT_FN(bch2_opt_disk_label),
         BCH_MEMBER_GROUP,              0,
         "(label)",     "Device label: position in the label tree"),
        (failure_domain,                u8,
         OPT_DEVICE | OPT_FORMAT | OPT_RUNTIME,
         OPT_STR_MEMBER(failure_domain),
         BCH2_NO_SB_OPT,                0,
         "(domain)",    "Failure domain: devices sharing a name are in the\n"
                        "same failure domain; allocation spreads replicas\n"
                        "across domains (a hard requirement for erasure\n"
                        "coded stripe blocks)"),
        (bucket_size,                   u32,
         OPT_DEVICE | OPT_HUMAN_READABLE | OPT_SB_FIELD_SECTORS,
         OPT_UINT(0, S64_MAX),
         BCH_MEMBER_BUCKET_SIZE,        0,
         "size",        "Specifies the bucket size; must be greater than the btree node size"),
        (durability,                    u8,
         OPT_DEVICE | OPT_RUNTIME | OPT_SB_FIELD_ONE_BIAS,
         OPT_UINT(0, BCH_MEMBER_DURABILITY_MAX - 1),
         BCH_MEMBER_DURABILITY,         1,
         "n",           "Data written to this device will be considered\n"
                        "to have already been replicated n times"),
        (data_allowed,                  u8,
         OPT_DEVICE | OPT_FORMAT,
         OPT_BITFIELD_MASK(__bch2_data_types,
                 BIT(BCH_DATA_journal) | BIT(BCH_DATA_btree) | BIT(BCH_DATA_user)),
         BCH_MEMBER_DATA_ALLOWED,       BIT(BCH_DATA_journal) | BIT(BCH_DATA_btree) | BIT(BCH_DATA_user),
         "types",       "Allowed data types for this device: journal, btree, and/or user"),
        (discard,                       u8,
         OPT_MOUNT | OPT_FS | OPT_DEVICE | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_MEMBER_DISCARD,            true,
         NULL,          "Enable discard/TRIM support"),
        (rotational,                    u8,
         OPT_DEVICE | OPT_RUNTIME,
         OPT_BOOL(),
         BCH_MEMBER_ROTATIONAL,         false,
         NULL,          "Disk is rotational; different behaviour for reconcile"),
        (btree_node_prefetch,           u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME | OPT_NODOC,
         OPT_BOOL(),
         BCH2_NO_SB_OPT,                true,
         NULL,          "BTREE_ITER_prefetch causes btree nodes to be\n"
                        " prefetched sequentially"),
        (btree_cache_shrinker_seeks,    u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(1, 100),
         BCH_SB_EXT_BTREE_CACHE_SHRINKER_SEEKS, 2,
         NULL,          "Shrinker cost of re-reading a btree node;\n"
                        " higher means the btree cache is evicted less\n"
                        " aggressively under memory pressure. Consider\n"
                        " raising on rotational storage, where re-reading\n"
                        " evicted nodes is expensive (pinned nodes use 4x)"),
        (dev_readahead,                 u64,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME | OPT_HUMAN_READABLE | OPT_SB_FIELD_SECTORS,
         OPT_UINT(0, BCH_SB_EXT_DEV_READAHEAD_MAX << 9),
         BCH_SB_EXT_DEV_READAHEAD,      SZ_2M,
         "size",        "Per-device readahead window size; summed across\n"
                        " all devices to set the filesystem readahead"),
        (ec_stripe_buf_limit,           u8,
         OPT_FS | OPT_MOUNT | OPT_RUNTIME,
         OPT_UINT(1, 25),
         BCH_SB_EXT_EC_STRIPE_BUF_LIMIT, 5,
         "%",           "Maximum percentage of total RAM for in-flight\n"
                        " EC stripe buffers"),
    }
}

macro_rules! __bch_opt_id_0 {
    ([$($acc:tt)*] $(($_name:tt$(, $($__rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_opt_id: u32 {
                $($acc)*
                $([<Opt_ $_name>],)*
                bch2_opts_nr,
            }
        }
    } };
}
BCH_OPTS!(__bch_opt_id_0 []);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_opts_mask {
    #[c("unsigned long d[BITS_TO_LONGS(bch2_opts_nr)]")]
    pub d: [core::ffi::c_ulong; bits_to_longs(c::bch2_opts_nr as usize)],
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

c_verbatim!(r#"
struct bch_status_fd;
"#);

c_opaque!(bch_status_fd);

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
     * see bch2_fs_parse_param(). @status is freed from the file's .release
     * method, so we hold a reference to the file of our own for as long as
     * the filesystem might still print there, and drop it in
     * bch2_fs_context_free().
     */
    pub status: *mut c::bch_status_fd,
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

c_verbatim!(r#"
struct bch_fs;

struct printbuf;
"#);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_option {
    pub attr: c::attribute,
    pub type_: c::opt_type,
    pub flags: c::opt_flags,
    pub min: u64,
    pub max: u64,

    pub choices: *const *const crate::util::ffi::c_char,
    pub choices_allowed_mask: u64,

    pub fn_: c::bch_opt_fn,

    pub hint: *const crate::util::ffi::c_char,
    pub help: *const crate::util::ffi::c_char,

    pub get_sb: Option<unsafe extern "C" fn(*const c::bch_sb) -> u64>,
    pub set_sb: Option<unsafe extern "C" fn(*mut c::bch_sb, u64)>,

    pub get_member: Option<unsafe extern "C" fn(*const c::bch_member) -> u64>,
    pub set_member: Option<unsafe extern "C" fn(*mut c::bch_member, u64)>,

    pub get_ext: Option<unsafe extern "C" fn(*const c::bch_sb_field_ext) -> u64>,
    pub set_ext: Option<unsafe extern "C" fn(*mut c::bch_sb_field_ext, u64)>,

    /* BCH_OPT_STR_MEMBER: the bch_member char[] field the string lives in */
    pub member_offset: core::ffi::c_uint,
    pub member_size: core::ffi::c_uint,
}
c_default!(bch_option);

c_verbatim!(r#"
struct bch_dev;

struct opt_change_scope;
"#);

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

// What Rust calls of opts.h: C gets these as prototypes, in opts_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    #[c("const char * const __bch2_btree_ids[]")]
    pub static __bch2_btree_ids: [*const crate::util::ffi::c_char; 0usize];
    #[c("const char * const bch2_d_types[]")]
    pub static bch2_d_types: [*const crate::util::ffi::c_char; 0usize];
    pub fn bch2_prt_jset_entry_type(arg1: *mut c::printbuf, arg2: c::bch_jset_entry_type);
    pub fn bch2_prt_data_type(arg1: *mut c::printbuf, arg2: c::bch_data_type);
    pub fn bch2_prt_compression_type(arg1: *mut c::printbuf, arg2: c::bch_compression_type);
    pub fn bch2_prt_str_hash_type(arg1: *mut c::printbuf, arg2: c::bch_str_hash_type);
    pub fn bch2_prt_reconcile_accounting_type(arg1: *mut c::printbuf, arg2: c::bch_reconcile_accounting_type);
    #[c("const struct bch_opts bch2_opts_default")]
    pub static bch2_opts_default: c::bch_opts;
    #[c("const struct bch_option bch2_opt_table[]")]
    pub static bch2_opt_table: [c::bch_option; 0usize];
    pub fn bch2_opt_defined_by_id(arg1: *const c::bch_opts, arg2: c::bch_opt_id) -> bool;
    pub fn bch2_opt_get_by_id(arg1: *const c::bch_opts, arg2: c::bch_opt_id) -> u64;
    pub fn bch2_opt_set_by_id(arg1: *mut c::bch_opts, arg2: c::bch_opt_id, arg3: u64);
    pub fn bch2_opts_from_sb(arg1: *mut c::bch_opts, arg2: *mut c::bch_sb) -> core::ffi::c_int;
    pub fn __bch2_opt_set_sb(arg1: *mut c::bch_sb, arg2: core::ffi::c_int, arg3: *const c::bch_option, arg4: u64, arg5: *const crate::util::ffi::c_char) -> bool;
    pub fn bch2_opt_set_sb(arg1: *mut c::bch_fs, arg2: *mut c::bch_dev, arg3: *const c::bch_option, arg4: u64, arg5: *const crate::util::ffi::c_char) -> bool;
    pub fn bch2_opt_lookup(arg1: *const crate::util::ffi::c_char) -> core::ffi::c_int;
    pub fn bch2_opt_parse(arg1: *mut c::bch_fs, arg2: *const c::bch_option, arg3: *const crate::util::ffi::c_char, arg4: *mut u64, arg5: *mut c::printbuf) -> core::ffi::c_int;
    pub fn bch2_opt_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: *mut c::bch_sb, arg4: *const c::bch_option, arg5: u64, arg6: core::ffi::c_uint);
    pub fn bch2_opt_hook_pre_set(arg1: *mut c::bch_fs, arg2: *mut c::bch_dev, arg3: u64, arg4: c::bch_opt_id, arg5: u64, arg6: bool, arg7: *mut c::opt_change_scope) -> core::ffi::c_int;
    pub fn bch2_opt_hook_post_set(arg1: *mut c::bch_fs, arg2: *mut c::bch_dev, arg3: u64, arg4: c::bch_opt_id, arg5: u64);
    pub fn bch2_parse_mount_opts(arg1: *mut c::bch_fs, arg2: *mut c::bch_opts, arg3: *mut c::printbuf, arg4: *mut crate::util::ffi::c_char, arg5: bool) -> core::ffi::c_int;
    pub fn bch2_opt_is_inode_opt(arg1: c::bch_opt_id) -> bool;
    pub fn bch2_opt_change_unlock(arg1: *mut c::bch_fs);
    pub fn bch2_opt_change_lock(arg1: *mut c::bch_fs);
}
