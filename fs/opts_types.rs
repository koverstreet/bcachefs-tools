// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::cstructs::c::BCH_INODE_OPTS;
use crate::c::{
    bch_member, bch_sb_field_ext,
    BCH_COMPRESSION_OPT_none, BCH_CSUM_OPT_crc32c, BCH_DATA_btree, BCH_DATA_journal,
    BCH_DATA_user, BCH_DEGRADED_ask, BCH_MEMBER_STATE_rw, BCH_ON_ERROR_fix_safe,
    BCH_REPLICAS_MAX, BCH_SB_SECTOR, BCH_STR_HASH_OPT_siphash,
    BCH_VERSION_UPGRADE_compatible, BCH_WRITE_DEGRADED_degraded,
};
use crate::types::bits_to_longs;
use core::ffi::c_ulong;
use cstruct_macros::{CStruct, c_const, c_enum, c_xmacro, Options, TypeInfo};

/*
 * Mount options; we also store defaults in the superblock.
 *
 * Also exposed via sysfs: if an option is writeable, and it's also stored in
 * the superblock, changing it via sysfs (currently? might change this) also
 * updates the superblock.
 *
 * A bch_opts says which of its options are defined, in its mask: so we can
 * pass the mount options to bch2_fs_alloc() as a whole struct, and then only
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

c_const! {
    /* how many options a bch_opts_mask has room for */
    pub const BCH_OPTS_MAX: u32 = 128;
}

/* options, by bch_opt_id: which are set */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_opts_mask {
    #[c("unsigned long d[BITS_TO_LONGS(BCH_OPTS_MAX)]")]
    pub d: [c_ulong; bits_to_longs(c::BCH_OPTS_MAX as usize)],
}

/*
 * The options, a field each: its #[opt] - its type, when it may be set, its
 * superblock field, its default - and its doc comment, its help.
 * #[derive(Options)] (cstruct-macros/src/options.rs) makes their ids,
 * bch_opt_id, in field order, and their table, opts.rs's OPTS. @defined:
 * those this bch_opts sets - mount options as parsed have only those given.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo, Options)]
pub struct bch_opts {
    /// Filesystem block size
    #[opt(uint(512, 1 << 15),
          flags = fs | format | human_readable | must_be_pow_2 | sb_field_sectors,
          sb = bch_sb::block_size,
          default = 4 << 10,
          hint = "size")]
    pub block_size: u16,

    /// Btree node size, default 256k
    #[opt(uint(512, 1 << 19),
          flags = fs | format | human_readable | must_be_pow_2 | sb_field_sectors,
          sb = bch_sb::btree_node_size,
          default = 256 << 10,
          hint = "size")]
    pub btree_node_size: u32,

    /// Action to take on filesystem error
    #[opt(str(bch_error_actions),
          flags = fs | format | mount | runtime,
          sb = bch_sb::error_action,
          default = BCH_ON_ERROR_fix_safe)]
    pub errors: u8,

    /// Number of consecutive write errors allowed before kicking out a device
    #[opt(uint(1, 300),
          flags = fs | format | mount | runtime,
          sb = bch_sb::write_error_timeout,
          default = 30)]
    pub write_error_timeout: u16,

    /// Number of metadata replicas (journal and btree)
    #[opt(uint(1, BCH_REPLICAS_MAX),
          flags = fs | format | mount_old | runtime,
          sb = bch_sb::meta_replicas_want,
          default = 1,
          hint = "#")]
    pub metadata_replicas: u8,

    /// Number of journal replicas, if not metadata_replicas
    #[opt(uint(0, BCH_REPLICAS_MAX),
          flags = fs | format | runtime,
          sb = bch_sb_field_ext::journal_replicas,
          hint = "#")]
    pub journal_replicas: u8,

    /// Number of data replicas (erasure coding currently caps this at 3, RAID6)
    #[opt(uint(1, BCH_REPLICAS_MAX),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::data_replicas_want,
          default = 1,
          hint = "#")]
    pub data_replicas: u8,

    /// Maximum size of checksummed/compressed extents
    #[opt(uint(4096, 2 << 20),
          flags = fs | format | human_readable | must_be_pow_2 | sb_field_sectors | sb_field_ilog2,
          sb = bch_sb::encoded_extent_max_bits,
          default = 256 << 10,
          hint = "size")]
    pub encoded_extent_max: u32,

    /// Checksum type for metadata writes
    #[opt(str(bch_csum_opt),
          flags = fs | format | mount_old | runtime,
          sb = bch_sb::meta_csum_type,
          default = BCH_CSUM_OPT_crc32c)]
    pub metadata_checksum: u8,

    /// Checksum type for data writes
    #[opt(str(bch_csum_opt),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::data_csum_type,
          default = BCH_CSUM_OPT_crc32c)]
    pub data_checksum: u8,

    /// Number of read retries on checksum error
    #[opt(uint(0, 32), flags = fs | format | mount | runtime, sb = bch_sb::csum_err_retry_nr, default = 3)]
    pub checksum_err_retry_nr: u8,

    /// Compression type for data writes
    #[opt(with(crate::data::compress::OPT),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::compression_type,
          default = BCH_COMPRESSION_OPT_none)]
    pub compression: u8,

    /// Compression type for background moves
    #[opt(with(crate::data::compress::OPT),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::background_compression_type,
          default = BCH_COMPRESSION_OPT_none)]
    pub background_compression: u8,

    /// Hash function for directory entries and xattrs
    #[opt(str(bch_str_hash_opts),
          flags = fs | format | mount | runtime,
          sb = bch_sb::str_hash_type,
          default = BCH_STR_HASH_OPT_siphash)]
    pub str_hash: u8,

    /// Device or label for metadata writes
    #[opt(with(crate::alloc::disk_groups::TARGET_OPT),
          flags = fs | format | mount_old | runtime,
          sb = bch_sb::metadata_target,
          hint = "(target)")]
    pub metadata_target: u16,

    /// Device or label for foreground writes
    #[opt(with(crate::alloc::disk_groups::TARGET_OPT),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::foreground_target,
          hint = "(target)")]
    pub foreground_target: u16,

    /// Device or label to move data to in the background
    #[opt(with(crate::alloc::disk_groups::TARGET_OPT),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::background_target,
          hint = "(target)")]
    pub background_target: u16,

    /// Device or label to promote data to on read
    #[opt(with(crate::alloc::disk_groups::TARGET_OPT),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::promote_target,
          hint = "(target)")]
    pub promote_target: u16,

    /// Enable erasure coding (RAID5/6; data replicas are capped at 3)
    #[opt(bool, flags = fs | inode | format | mount_old | runtime, sb = bch_sb::erasure_code)]
    pub erasure_code: u16,

    /// Cap data blocks per EC stripe (0 = use all active devs)
    #[opt(uint(0, 15), flags = fs | format | mount | runtime, sb = bch_sb::ec_max_data_blocks)]
    pub ec_max_data_blocks: u8,

    /// Dirent lookups are casefolded
    #[opt(bool, flags = fs | inode | format, sb = bch_sb::casefold)]
    pub casefold: u8,

    /// Disable casefolding filesystem wide
    #[opt(bool, flags = fs | mount)]
    pub casefold_disabled: u8,

    /// Constrain inode numbers to 32 bits
    #[opt(bool, flags = fs | inode | format | mount | runtime, sb = bch_sb::inode_32bit)]
    pub inodes_32bit: u8,

    /// Shard new inode numbers by CPU id
    #[opt(uint(0, 8), flags = fs | format, sb = bch_sb::shard_inums_nbits)]
    pub shard_inode_numbers_bits: u8,

    /// Percentage of disk space to reserve for copygc
    #[opt(uint(5, 20),
          flags = fs | format | mount | runtime,
          sb = bch_sb::gc_reserve,
          default = 8,
          hint = "%")]
    pub gc_reserve_percent: u8,

    /// Amount of disk space to reserve for copygc
    /// Takes precedence over gc_reserve_percent if set
    #[opt(uint(0, u64::MAX),
          flags = fs | format | mount | runtime | human_readable | sb_field_sectors,
          sb = bch_sb::gc_reserve_bytes,
          hint = "%")]
    pub gc_reserve_bytes: u64,

    /// Percentage of disk space to reserve for superuser
    #[opt(uint(0, 100), flags = fs | format | mount, sb = bch_sb::root_reserve, hint = "%")]
    pub root_reserve_percent: u8,

    /// Store full 128 bits of cryptographic MACs, instead of 80
    #[opt(bool, flags = fs | format | mount | runtime, sb = bch_sb::_128_bit_macs / set_128_bit_macs)]
    pub wide_macs: u8,

    /// Enable inline data extents
    #[opt(bool, flags = fs | mount | runtime, default = true)]
    pub inline_data: u8,

    /// Promote whole extents, instead of just part being read
    #[opt(bool, flags = fs | mount | runtime, sb = bch_sb::promote_whole_extents, default = true)]
    pub promote_whole_extents: u8,

    /// Enable POSIX acls
    #[opt(bool, flags = fs | format | mount, sb = bch_sb::posix_acl, default = true)]
    pub acl: u8,

    /// Enable user quotas
    #[opt(bool, flags = fs | format | mount, sb = bch_sb::usrquota)]
    pub usrquota: u8,

    /// Enable group quotas
    #[opt(bool, flags = fs | format | mount, sb = bch_sb::grpquota)]
    pub grpquota: u8,

    /// Enable project quotas
    #[opt(bool, flags = fs | format | mount, sb = bch_sb::prjquota)]
    pub prjquota: u8,

    /// Allow mounting in degraded mode
    #[opt(str(bch_degraded_actions),
          flags = fs | mount,
          sb = bch_sb::degraded_action,
          default = BCH_DEGRADED_ask)]
    pub degraded: u8,

    /// Write fewer copies than asked for rather than returning ENOSPC; the default does so only while a device is missing or not read-write
    #[opt(str(bch_write_degraded_actions),
          flags = fs | mount | format | runtime,
          sb = bch_sb::write_degraded_action,
          default = BCH_WRITE_DEGRADED_degraded)]
    pub write_degraded: u8,

    /// Seconds to wait at mount for member devices that haven't appeared yet, before applying the degraded action; 0 means use the built-in default
    #[opt(uint(0, 3600), flags = fs | mount | format | runtime, sb = bch_sb_field_ext::missing_dev_timeout)]
    pub missing_dev_timeout: u32,

    /// Trust udev when scanning for member devices
    #[opt(bool, flags = mount, default = true)]
    pub mount_trusts_udev: u8,

    /// Don't kick drives out when splitbrain detected
    #[opt(bool, flags = fs | mount)]
    pub no_splitbrain_check: u8,

    /// Don't fail reading the superblock due to incompatible version
    #[opt(bool, flags = hidden)]
    pub no_version_check: u8,

    /// Extra debugging information during mount/recovery
    #[opt(bool, flags = fs | mount | runtime, default = BCACHEFS_VERBOSE_DEFAULT)]
    pub verbose: u8,

    /// Delay in milliseconds before automatic journal commits
    #[opt(uint(1, u32::MAX), flags = fs | mount | runtime, sb = bch_sb::journal_flush_delay, default = 1000)]
    pub journal_flush_delay: u32,

    /// Disable journal flush on sync/fsync
    /// If enabled, writes can be lost, but only since the
    /// last journal write (default 1 second)
    #[opt(bool, flags = fs | mount | runtime, sb = bch_sb::journal_flush_disabled)]
    pub journal_flush_disabled: u8,

    /// Issue writes from background data moves (copygc,
    /// rebalance) with FUA, making them durable on
    /// completion rather than relying on the journal's
    /// periodic cache flush
    #[opt(bool, flags = fs | mount | runtime, sb = bch_sb::move_writes_fua)]
    pub move_writes_fua: u8,

    /// Delay in milliseconds before automatic journal reclaim
    #[opt(uint(0, u32::MAX), flags = fs | mount | runtime, sb = bch_sb::journal_reclaim_delay, default = 100)]
    pub journal_reclaim_delay: u32,

    /// Delay seconds before writing back dirty data, overriding vm sysctls
    #[opt(uint(0, u16::MAX), flags = fs | mount | runtime, sb = bch_sb::writeback_timeout)]
    pub writeback_timeout: u16,

    /// Maximum Amount of IO to keep in flight by the move path
    #[opt(uint(1024, u32::MAX), flags = human_readable | fs | mount | runtime | nodoc, default = 64 << 20)]
    pub move_bytes_in_flight: u32,

    /// Maximum number of IOs to keep in flight by the move path
    #[opt(uint(1, 1024), flags = fs | mount | runtime | nodoc, default = 64)]
    pub move_ios_in_flight: u32,

    /// Run fsck on mount
    #[opt(bool, flags = fs | mount)]
    pub fsck: u8,

    /// Maximum percentage of system ram fsck is allowed to pin
    #[opt(uint(20, 70), flags = fs | mount, default = 50)]
    pub fsck_memory_usage_percent: u8,

    /// Fix errors during fsck without asking
    #[opt(with(crate::opts::FIX_ERRORS), flags = fs | mount, default = FSCK_FIX_exit)]
    pub fix_errors: u8,

    /// Ratelimit error messages during fsck
    #[opt(bool, flags = fs | mount | runtime, default = RATELIMIT_ERRORS_DEFAULT)]
    pub ratelimit_errors: u8,

    /// Disable commit-time-only bkey validation;
    /// for error injection tools, which must be able
    /// to write the states fsck is tested against
    #[opt(bool, flags = fs | mount)]
    pub no_commit_validate: u8,

    /// Super read only mode - no writes at all will be issued,
    /// even if we have to replay the journal
    #[opt(bool, flags = fs | mount)]
    pub nochanges: u8,

    /// Exit recovery immediately prior to journal replay
    #[opt(bool, flags = fs | mount)]
    pub norecovery: u8,

    /// Rewind journal
    #[opt(uint(0, u64::MAX), flags = fs | mount)]
    pub journal_rewind: u64,

    /// Percentage of filesystem capacity to leave undiscarded for journal rewind
    #[opt(uint(0, 10), flags = fs | mount, sb = bch_sb_field_ext::discard_buffer, default = 4)]
    pub journal_rewind_discard_buffer_percent: u8,

    /// Scrub data written in the last few journal entries during recovery
    #[opt(str(bch_scrub_journal_opts), flags = fs | mount, sb = bch_sb::scrub_journal)]
    pub scrub_recent_journal_entries: u8,

    /// Maximum time in seconds the journal scrub will rewind (default 10)
    #[opt(uint(0, 3600),
          flags = fs | mount | format,
          sb = bch_sb_field_ext::scrub_max_rewind_secs,
          default = 10)]
    pub scrub_journal_max_rewind_secs: u32,

    /// Recovery passes to run explicitly
    #[opt(bitfield(bch_recovery_pass), flags = fs | mount)]
    pub recovery_passes: u64,

    /// Recovery passes to exclude
    #[opt(bitfield(bch_recovery_pass), flags = fs | mount)]
    pub recovery_passes_exclude: u64,

    /// Exit recovery after specified pass
    #[opt(str_nolimit(bch_recovery_pass), flags = fs | mount)]
    pub recovery_pass_last: u8,

    /// Don't run repair passes, whether the superblock had them scheduled or something schedules them mid-mount; they stay scheduled for the next mount
    #[opt(bool, flags = fs | mount)]
    pub recovery_passes_skip_scheduled: u8,

    /// Don't free journal entries/keys, scanned btree nodes after startup
    #[opt(bool)]
    pub retain_recovery_info: u8,

    /// Read all journal entries, not just dirty ones
    #[opt(bool)]
    pub read_entire_journal: u8,

    /// Only read the journal, skip the rest of recovery
    #[opt(bool)]
    pub read_journal_only: u8,

    /// Log transaction function names in journal
    #[opt(bool,
          flags = fs | format | mount | runtime | nodoc,
          sb = bch_sb::journal_transaction_names,
          default = true)]
    pub journal_transaction_names: u8,

    /// Default timeout in seconds for stuck allocator messages
    #[opt(uint(0, u16::MAX),
          flags = fs | format | mount | runtime | nodoc,
          sb = bch_sb::allocator_stuck_timeout,
          default = 30)]
    pub allocator_stuck_timeout: u16,

    /// Don't open device in exclusive mode
    #[opt(bool, flags = fs | mount)]
    pub noexcl: u8,

    /// Use O_DIRECT (userspace only)
    #[opt(bool, flags = fs | mount | nodoc, default = true)]
    pub direct_io: u8,

    /// Sector offset of superblock
    #[opt(uint(0, i64::MAX), flags = mount, default = BCH_SB_SECTOR, hint = "offset")]
    pub sb: u64,

    #[opt(bool, flags = fs | mount | hidden)]
    pub read_only: u8,

    /// Don't start filesystem, only open devices
    #[opt(bool)]
    pub nostart: u8,

    #[opt(bool, flags = hidden)]
    pub will_not_start: u8,

    /// Reconstruct alloc btree
    #[opt(bool, flags = fs | mount)]
    pub dangerously_reconstruct_alloc: u8,

    /// Set superblock to latest version,
    /// allowing any new features to be used
    #[opt(str(bch_version_upgrade_opts),
          flags = fs | mount | runtime,
          sb = bch_sb::version_upgrade,
          default = BCH_VERSION_UPGRADE_compatible)]
    pub version_upgrade: u8,

    /// Pointer to a struct stdio_redirect
    #[opt(uint(0, i64::MAX))]
    pub stdio: u64,

    #[opt(bool, flags = inode | nodoc)]
    pub project: u8,

    /// Nocow mode: Writes will be done in place when possible.
    /// Snapshots and reflink will still cause writes to be COW
    /// Implicitly disables data checksumming, compression and encryption
    #[opt(bool, flags = fs | format | mount | runtime | inode, sb = bch_sb::nocow)]
    pub nocow: u8,

    /// Enable nocow mode: enables runtime locking in
    /// data move path needed if nocow will ever be in use
    #[opt(bool, flags = fs | mount | nodoc, default = true)]
    pub nocow_enabled: u8,

    /// Enable copygc: disable for debugging, or to
    /// quiet the system when doing performance testing
    #[opt(bool, flags = fs | mount | runtime, default = true)]
    pub copygc_enabled: u8,

    /// Enable reconcile: disable for debugging, or to
    /// quiet the system when doing performance testing
    #[opt(bool, flags = fs | mount | runtime, default = true)]
    pub reconcile_enabled: u8,

    /// Enable reconcile while on mains power only
    #[opt(bool, flags = fs | mount | runtime, sb = bch_sb::rebalance_ac_only)]
    pub reconcile_on_ac_only: u8,

    /// Enable automatic snapshot deletion: disable for debugging, or to
    /// quiet the system when doing performance testing
    #[opt(bool, flags = fs | mount | runtime, default = true)]
    pub auto_snapshot_deletion: u8,

    /// Skip submit_bio() for data reads and writes, for performance testing purposes
    #[opt(bool, flags = mount | nodoc)]
    pub no_data_io: u8,

    /// Device state
    #[opt(str(bch_member_state),
          flags = device | runtime,
          sb = bch_member::state,
          default = BCH_MEMBER_STATE_rw)]
    pub state: u64,

    /// Device label: position in the label tree
    #[opt(with(crate::alloc::disk_groups::LABEL_OPT),
          flags = device | format | runtime,
          sb = bch_member::group,
          hint = "(label)")]
    pub label: u16,

    /// Failure domain: devices sharing a name are in the
    /// same failure domain; allocation spreads replicas
    /// across domains (a hard requirement for erasure
    /// coded stripe blocks)
    #[opt(str_member(failure_domain), flags = device | format | runtime, hint = "(domain)")]
    pub failure_domain: u8,

    /// Specifies the bucket size; must be greater than the btree node size
    #[opt(uint(0, i64::MAX),
          flags = device | human_readable | sb_field_sectors,
          sb = bch_member::bucket_size,
          hint = "size")]
    pub bucket_size: u32,

    /// Data written to this device will be considered
    /// to have already been replicated n times
    #[opt(uint(0, bch_member::DURABILITY_MAX - 1),
          flags = device | runtime | sb_field_one_bias,
          sb = bch_member::durability,
          default = 1,
          hint = "n")]
    pub durability: u8,

    /// Allowed data types for this device: journal, btree, and/or user
    #[opt(bitfield(bch_data_type, 1 << BCH_DATA_journal | 1 << BCH_DATA_btree | 1 << BCH_DATA_user),
          flags = device | format,
          sb = bch_member::data_allowed,
          default = 1 << BCH_DATA_journal | 1 << BCH_DATA_btree | 1 << BCH_DATA_user,
          hint = "types")]
    pub data_allowed: u8,

    /// Enable discard/TRIM support
    #[opt(bool, flags = mount | fs | device | runtime, sb = bch_member::discard, default = true)]
    pub discard: u8,

    /// Disk is rotational; different behaviour for reconcile
    #[opt(bool, flags = device | runtime, sb = bch_member::rotational)]
    pub rotational: u8,

    /// BTREE_ITER_prefetch causes btree nodes to be
    ///  prefetched sequentially
    #[opt(bool, flags = fs | mount | runtime | nodoc, default = true)]
    pub btree_node_prefetch: u8,

    /// Shrinker cost of re-reading a btree node;
    ///  higher means the btree cache is evicted less
    ///  aggressively under memory pressure. Consider
    ///  raising on rotational storage, where re-reading
    ///  evicted nodes is expensive (pinned nodes use 4x)
    #[opt(uint(1, 100),
          flags = fs | mount | runtime,
          sb = bch_sb_field_ext::btree_cache_shrinker_seeks,
          default = 2)]
    pub btree_cache_shrinker_seeks: u8,

    /// Per-device readahead window size; summed across
    ///  all devices to set the filesystem readahead
    #[opt(uint(0, bch_sb_field_ext::DEV_READAHEAD_MAX << 9),
          flags = fs | mount | runtime | human_readable | sb_field_sectors,
          sb = bch_sb_field_ext::dev_readahead,
          default = 2 << 20,
          hint = "size")]
    pub dev_readahead: u64,

    /// Maximum percentage of total RAM for in-flight
    ///  EC stripe buffers
    #[opt(uint(1, 25),
          flags = fs | mount | runtime,
          sb = bch_sb_field_ext::ec_stripe_buf_limit,
          default = 5,
          hint = "%")]
    pub ec_stripe_buf_limit: u8,

    pub defined: bch_opts_mask,
}

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

