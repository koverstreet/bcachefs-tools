// SPDX-License-Identifier: GPL-2.0
//! Options: mount options, the sysfs options/ files, format options, and the
//! filesystem defaults the superblock keeps - one list, bch_opts' fields
//! (opts_types.rs), and OPTS, the table #[derive(Options)] makes from them.
//!
//! Terms:
//! - An option, Opt: what its field's #[opt] says of one - its type and
//!   bounds, when it may be set (flags, OPT_*), where the superblock keeps
//!   it, if it does. OPTS is indexed by OptId.
//! - A value is a u64, whatever the option's type: a bool's 0 or 1, a string
//!   option's index into its choices, a bitfield's bits.
//! - bch_opts: a value for each option, and whether it's defined - mount
//!   options as parsed have only those given; c->opts has all of them.
//!   DEFAULTS is the #[opt]s' defaults, all defined. Each option's field
//!   has a getter, name(), a setter that defines it, set_name(), and
//!   name_or_default().
//! - Where the superblock keeps an option: a bitfield of the superblock
//!   itself, of its ext section, or of a member - its #[opt] names the
//!   field's getter, and OptHost finds the struct it's on in a superblock.
//!   The field may hold the value scaled: OPT_SB_FIELD_SECTORS (>> 9),
//!   _ILOG2, _ONE_BIAS (0 is unset).
//!
//! Changing an option on a running filesystem is an OptChange, and the
//! hooks either side of the change: opts/change.rs.
//!
//! Also here, as opts.c had them: the dirent types' names. The format's
//! enums' are theirs - util/names.rs.

#![allow(non_camel_case_types)]

use crate::fs::Dev;
use crate::c;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::inode_opts::InodeOpt;
use crate::sb::io::SbState;
use crate::prt;
use crate::util::vstructs::{bytes_of_mut, cast_plain_mut, plain_bytes_mut, Unvalidated};
use crate::sb::members;
use crate::util::log::{log_no_fs, LogLevel};
use crate::util::names::{self, Names};
use crate::util::{HumanReadable, Printbuf};
use crate::util::printbuf::parse_human_u64;
use crate::util::sysfs::parse_int;
use core::ffi::{CStr, c_ulong};
use crate::cstructs::c::InodeOptId_arms;
use crate::c::{bch_member, bch_sb_field_ext, CompressionOpt, CsumOpt, ErrorAction, StrHashOpt, VersionUpgradeOpt, WriteDegradedAction, MemberState, BCH_REPLICAS_MAX, BCH_SB_SECTOR};
use crate::format::DataType;
use crate::types::bits_to_longs;
use cstruct_macros::{MaybeZeroable, bch_enum, Options, TypeInfo};

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
#[bch_enum(flags)]
#[repr(u32)]
pub enum OptFlags {
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

/* the ratelimit_errors option's default: on in the kernel */
pub const RATELIMIT_ERRORS_DEFAULT: bool = cfg!(__KERNEL__);

/* the verbose option's default: on in debug builds */
pub const BCACHEFS_VERBOSE_DEFAULT: bool = cfg!(CONFIG_BCACHEFS_DEBUG);

/* bch2_opt_to_text()'s flags */
pub const OPT_SHOW_FULL_LIST: u32 = 1 << 0;
pub const OPT_SHOW_MOUNT_STYLE: u32 = 1 << 1;

#[bch_enum(closed)]
#[prefix(FSCK_FIX_)]
#[repr(u32)]
pub enum FsckErrOpt {
    exit = 0,
    yes = 1,
    no = 2,
    ask = 3,
}

/* how many options a bch_opts_mask has room for */
pub const BCH_OPTS_MAX: u32 = 128;

/* options, by OptId: which are set */
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct bch_opts_mask {
    pub d: [c_ulong; bits_to_longs(c::BCH_OPTS_MAX as usize)],
}

/// A bch_opts_mask that changes while it's read: PublishedOpts', and
/// c->mount_opts.
pub type AtomicOptsMask = crate::data_structures::bitmap::AtomicBitArray<{ bits_to_longs(c::BCH_OPTS_MAX as usize) }>;

/*
 * The options, a field each: its #[opt] - its type, when it may be set, its
 * superblock field, its default - and its doc comment, its help.
 * #[derive(Options)] (cstruct-macros/src/options.rs) makes their ids,
 * OptId, in field order, and their table, opts.rs's OPTS. @defined:
 * those this bch_opts sets - mount options as parsed have only those given.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo, Options)]
pub struct bch_opts {
    /// Filesystem block size
    #[opt(uint(512, 1 << 15),
          flags = fs | format | human_readable | must_be_pow_2 | sb_field_sectors,
          sb = bch_sb.block_size,
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
    #[opt(str(ErrorAction),
          flags = fs | format | mount | runtime,
          sb = bch_sb::error_action,
          default = ErrorAction::fix_safe.0)]
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
    #[opt(str(CsumOpt),
          flags = fs | format | mount_old | runtime,
          sb = bch_sb::meta_csum_type,
          default = CsumOpt::crc32c.0)]
    pub metadata_checksum: u8,

    /// Checksum type for data writes
    #[opt(str(CsumOpt),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::data_csum_type,
          default = CsumOpt::crc32c.0)]
    pub data_checksum: u8,

    /// Number of read retries on checksum error
    #[opt(uint(0, 32), flags = fs | format | mount | runtime, sb = bch_sb::csum_err_retry_nr, default = 3)]
    pub checksum_err_retry_nr: u8,

    /// Compression type for data writes
    #[opt(with(crate::data::compress::OPT),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::compression_type,
          default = CompressionOpt::none.0)]
    pub compression: u8,

    /// Compression type for background moves
    #[opt(with(crate::data::compress::OPT),
          flags = fs | inode | format | mount_old | runtime,
          sb = bch_sb::background_compression_type,
          default = CompressionOpt::none.0)]
    pub background_compression: u8,

    /// Hash function for directory entries and xattrs
    #[opt(str(StrHashOpt),
          flags = fs | format | mount | runtime,
          sb = bch_sb::str_hash_type,
          default = StrHashOpt::siphash.0)]
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
    #[opt(str(DegradedAction),
          flags = fs | mount,
          sb = bch_sb::degraded_action,
          default = c::DegradedAction::ask.0)]
    pub degraded: u8,

    /// Write fewer copies than asked for rather than returning ENOSPC; the default does so only while a device is missing or not read-write
    #[opt(str(WriteDegradedAction),
          flags = fs | mount | format | runtime,
          sb = bch_sb::write_degraded_action,
          default = WriteDegradedAction::degraded.0)]
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
    #[opt(with(crate::opts::FIX_ERRORS), flags = fs | mount, default = (FsckErrOpt::exit as u32))]
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
    #[opt(str(ScrubJournalOpt), flags = fs | mount, sb = bch_sb::scrub_journal)]
    pub scrub_recent_journal_entries: u8,

    /// Maximum time in seconds the journal scrub will rewind (default 10)
    #[opt(uint(0, 3600),
          flags = fs | mount | format,
          sb = bch_sb_field_ext::scrub_max_rewind_secs,
          default = 10)]
    pub scrub_journal_max_rewind_secs: u32,

    /// Recovery passes to run explicitly
    #[opt(bitfield(RecoveryPass), flags = fs | mount)]
    pub recovery_passes: u64,

    /// Recovery passes to exclude
    #[opt(bitfield(RecoveryPass), flags = fs | mount)]
    pub recovery_passes_exclude: u64,

    /// Exit recovery after specified pass
    #[opt(str_nolimit(RecoveryPass), flags = fs | mount)]
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
    #[opt(str(VersionUpgradeOpt),
          flags = fs | mount | runtime,
          sb = bch_sb::version_upgrade,
          default = VersionUpgradeOpt::compatible.0)]
    pub version_upgrade: u8,

    /// Pointer to a StdioRedirect (os/linux_kernel/thread_with_file.rs)
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
    #[opt(str(MemberState),
          flags = device | runtime,
          sb = bch_member::state,
          default = MemberState::rw.0)]
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
          sb = bch_member.bucket_size,
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
    #[opt(bitfield(DataType, 1 << DataType::journal.0 | 1 << DataType::btree.0 | 1 << DataType::user.0),
          flags = device | format,
          sb = bch_member::data_allowed,
          default = 1 << DataType::journal.0 | 1 << DataType::btree.0 | 1 << DataType::user.0,
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
    ([$($acc:tt)*] $(($_name:tt, $_nr:expr, [$_bits:tt])),* $(,)?) => { ::paste::paste! {
        InodeOptId_arms!(__bch_inode_opts_1 [
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
        #[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
        pub struct bch_inode_opts {
            $($acc)*
            pub __bch_inode_opts_bits: [u8; ([$(stringify!($_name)),*].len() * 1).div_ceil(8)],

            pub change_cookie: u32,
        }
    } };
}
InodeOptId_arms!(__bch_inode_opts_0 [pub __bch_inode_opts_align: [u64; 0],]);

pub const NR: usize = OptId::NR as usize;

/// By OptId: bch_opts' #[opt]s.
pub static OPTS: [Opt; NR] = c::bch_opts::TABLE;

/// Each option's default, all defined.
pub const DEFAULTS: c::bch_opts = c::bch_opts::DEFAULTS;


// ── Names ────────────────────────────────────────────────────────────────

/// @t's name - "(invalid data type)" if it isn't one: bch2_data_type_str().
pub fn data_type_str(t: DataType) -> &'static str {
    t.name().unwrap_or("(invalid data type)")
}

/// The dirent d_types' names, by DT_*.
pub const D_TYPES: [&str; c::BCH_DT_MAX as usize] = {
    let mut t = [""; c::BCH_DT_MAX as usize];
    t[c::DT_UNKNOWN as usize] = "unknown";
    t[c::DT_FIFO as usize]    = "fifo";
    t[c::DT_CHR as usize]     = "chr";
    t[c::DT_DIR as usize]     = "dir";
    t[c::DT_BLK as usize]     = "blk";
    t[c::DT_REG as usize]     = "reg";
    t[c::DT_LNK as usize]     = "lnk";
    t[c::DT_SOCK as usize]    = "sock";
    t[c::DT_WHT as usize]     = "whiteout";
    t[c::DT_SUBVOL as usize]  = "subvol";
    t
};

// ── The options ──────────────────────────────────────────────────────────

/// An option with a type of its own: OPT_FN().
pub struct OptFn {
    /// @val is None for an option given without one; Err(option_needs_open_fs)
    /// for one that can't be parsed without the filesystem.
    pub parse:    fn(fs: Option<&Fs>, val: Option<&[u8]>, err: Option<&mut Printbuf>) -> Result<u64, BchError>,
    /// Against @fs, or without one @sb: a target's, a label's names are
    /// theirs.
    pub to_text:  fn(out: &mut Printbuf, fs: Option<&Fs>, sb: Option<&c::bch_sb<Unvalidated>>, v: u64),
    pub validate: Option<fn(v: u64, err: Option<&mut Printbuf>) -> Result<(), BchError>>,
}

pub enum OptType {
    Bool,
    Uint,
    /// An index into its choices.
    Str(&'static [&'static str]),
    /// A bit per choice, of those @allowed.
    Bitfield { choices: &'static [&'static str], allowed: u64 },
    Fn(&'static OptFn),
    /// A string in a member's char[] field, which it's set and shown through:
    /// not a value.
    StrMember {
        get:     fn(&c::bch_member) -> &[u8],
        get_mut: fn(&mut c::bch_member) -> &mut [u8],
        size:    usize,
    },
}

pub(crate) const fn size_of_field<T, F>(_: fn(&T) -> &F) -> usize {
    size_of::<F>()
}

pub struct Opt {
    pub id:    OptId,
    pub name:  &'static str,
    pub flags: c::OptFlags,
    pub ty:    OptType,
    /// Bounds, inclusive: max 0 is none.
    pub min:   u64,
    pub max:   u64,
    /// Its superblock field, if it has one.
    pub sb:    Option<&'static dyn SbOpt>,
    pub hint:  Option<&'static str>,
    pub help:  Option<&'static str>,
}

/// An option's superblock field, as stored - before the OPT_SB_FIELD_*
/// scaling.
pub trait SbOpt: Sync {
    /// A member's: a device option's.
    fn member(&self) -> bool;
    /// None if the struct it's on isn't there: no ext section, no member @dev.
    fn get(&self, sb: &c::bch_sb<Unvalidated>, dev: u32) -> Option<u64>;
    /// Whether it changed: None if the struct it's on isn't there, Err if @v
    /// doesn't fit in the field.
    fn set(&self, sb: &mut c::bch_sb_buf, dev: u32, v: u64) -> Result<Option<bool>, BchError>;
}

/// What an option's superblock field is on, found in a superblock: the
/// superblock's head, its ext section, or member @dev. Read from one that
/// may not be valid - every superblock is an Unvalidated one - and written
/// in one that is.
pub trait OptHost: 'static {
    const MEMBER: bool;
    fn with<R>(sb: &c::bch_sb<Unvalidated>, dev: u32, f: impl FnOnce(&Self) -> R) -> Option<R>;
    fn with_mut<R>(sb: &mut c::bch_sb_buf, dev: u32, f: impl FnOnce(&mut Self) -> R) -> Option<R>;
}

impl OptHost for c::bch_sb__head {
    const MEMBER: bool = false;

    fn with<R>(sb: &c::bch_sb<Unvalidated>, _: u32, f: impl FnOnce(&Self) -> R) -> Option<R> {
        Some(f(Unvalidated::head(sb)))
    }

    fn with_mut<R>(sb: &mut c::bch_sb_buf, _: u32, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        cast_plain_mut(bytes_of_mut(sb.sb_mut())).map(f)
    }
}

/// Read, a copy: a section written before the struct grew has its missing
/// fields zero, as a validated one's arm reads.
impl OptHost for c::bch_sb_field_ext {
    const MEMBER: bool = false;

    fn with<R>(sb: &c::bch_sb<Unvalidated>, _: u32, f: impl FnOnce(&Self) -> R) -> Option<R> {
        let src = Unvalidated::field(sb, c::SbFieldType::ext)?.as_bytes();
        let mut ext = Self::default();
        let dst = plain_bytes_mut(&mut ext);
        let n = src.len().min(dst.len());
        dst[..n].copy_from_slice(&src[..n]);
        Some(f(&ext))
    }

    fn with_mut<R>(sb: &mut c::bch_sb_buf, _: u32, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        sb.field_mut::<Self>().map(f)
    }
}

/// A member that's alive: bch2_member_exists().
impl OptHost for c::bch_member {
    const MEMBER: bool = true;

    fn with<R>(sb: &c::bch_sb<Unvalidated>, dev: u32, f: impl FnOnce(&Self) -> R) -> Option<R> {
        members::sb_member(sb, dev).filter(|m| m.alive()).map(|m| f(&m))
    }

    fn with_mut<R>(sb: &mut c::bch_sb_buf, dev: u32, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        members::sb_member(sb.sb(), dev).filter(|m| m.alive())?;
        sb.member_mut(dev).map(f)
    }
}

/// A superblock field, by its accessors - a bitfield's, or an integer
/// field's: set is Err for a value that doesn't fit.
pub struct Field<T: ?Sized> {
    pub get: fn(&T) -> u64,
    pub set: fn(&mut T, u64) -> Result<(), ()>,
}

impl<T: OptHost + ?Sized> SbOpt for Field<T> {
    fn member(&self) -> bool {
        T::MEMBER
    }

    fn get(&self, sb: &c::bch_sb<Unvalidated>, dev: u32) -> Option<u64> {
        T::with(sb, dev, |t| (self.get)(t))
    }

    fn set(&self, sb: &mut c::bch_sb_buf, dev: u32, v: u64) -> Result<Option<bool>, BchError> {
        T::with_mut(sb, dev, |t| {
            let changed = v != (self.get)(t);
            (self.set)(t, v)
                .map(|()| changed)
                .map_err(|()| Errcode::BCH_ERR_ERANGE_option_too_big.into())
        }).transpose()
    }
}

/// fix_errors: given without a value, yes.
pub(crate) const FIX_ERRORS: OptFn = OptFn {
    parse: |_, val, err| {
        let Some(val) = val else {
            return Ok((c::FsckErrOpt::yes as u32).into());
        };
        let choices: &[&str] = crate::names_by_value!(c::FsckErrOpt);
        choices.iter().position(|n| n.as_bytes() == val).map(|v| v as u64).ok_or_else(|| {
            if let Some(err) = err {
                write!(err, "fix_errors: invalid selection");
            }
            c::EINVAL.into()
        })
    },
    to_text: |out, _, _, v| names::prt_name(out, c::FsckErrOpt::ENTRIES, v),
    validate: None,
};

impl OptId {
    pub fn opt(self) -> &'static Opt {
        &OPTS[self.0 as usize]
    }

    pub fn inode_opt(self) -> Option<InodeOpt> {
        InodeOpt::from_opt_id(self)
    }
}

impl Opt {
    pub fn has(&self, flag: c::OptFlags) -> bool {
        self.flags.intersects(flag)
    }

    /// Whether the superblock has it: a field, or a member's string.
    pub fn in_sb(&self) -> bool {
        self.sb.is_some() || matches!(self.ty, OptType::StrMember { .. })
    }

    /// Its superblock field, if it's the superblock's or the ext section's:
    /// a filesystem default.
    pub fn fs_field(&self) -> Option<&'static dyn SbOpt> {
        self.sb.filter(|f| !f.member())
    }

    /// Its superblock field, if it's a member's.
    pub fn member_field(&self) -> Option<&'static dyn SbOpt> {
        self.sb.filter(|f| f.member())
    }

    /// The choices of a string or bitfield option.
    pub fn choices(&self) -> &'static [&'static str] {
        match self.ty {
            OptType::Str(c) | OptType::Bitfield { choices: c, .. } => c,
            _ => &[],
        }
    }

    /// Its value from its superblock field's: the field may hold it scaled.
    pub fn from_sb_field(&self, v: u64) -> u64 {
        let mut v = v;
        if self.has(c::OptFlags::OPT_SB_FIELD_ONE_BIAS) {
            v = v.wrapping_sub(1);
        }
        if self.has(c::OptFlags::OPT_SB_FIELD_ILOG2) {
            v = 1u64.checked_shl(v as u32).unwrap_or(0);
        }
        if self.has(c::OptFlags::OPT_SB_FIELD_SECTORS) {
            v <<= 9;
        }
        v
    }

    /// Its superblock field's value for @v: from_sb_field()'s inverse.
    pub fn to_sb_field(&self, v: u64) -> u64 {
        let mut v = v;
        if self.has(c::OptFlags::OPT_SB_FIELD_SECTORS) {
            v >>= 9;
        }
        if self.has(c::OptFlags::OPT_SB_FIELD_ILOG2) {
            v = v.checked_ilog2().unwrap_or(0).into();
        }
        if self.has(c::OptFlags::OPT_SB_FIELD_ONE_BIAS) {
            v = v.wrapping_add(1);
        }
        v
    }

    /// A BCH_OPT_STR_MEMBER option's value: the string in member @m's field,
    /// up to a NUL.
    pub fn member_str<'a>(&self, m: &'a c::bch_member) -> &'a [u8] {
        let OptType::StrMember { get, .. } = self.ty else {
            panic!("{}: not a member string option", self.name);
        };
        get(m).split(|&b| b == 0).next().unwrap_or_default()
    }
}

// ── bch_opts ─────────────────────────────────────────────────────────────

impl c::bch_opts_mask {
    pub const fn test(&self, id: OptId) -> bool {
        let (i, bits) = (id.0 as usize, core::ffi::c_ulong::BITS as usize);
        self.d[i / bits] >> (i % bits) & 1 != 0
    }

    pub const fn set(&mut self, id: OptId) {
        let (i, bits) = (id.0 as usize, core::ffi::c_ulong::BITS as usize);
        self.d[i / bits] |= 1 << (i % bits);
    }

    /// A copy of @mask, a word at a time.
    pub fn load(mask: &c::AtomicOptsMask) -> Self {
        let words = mask.load();
        Self { d: core::array::from_fn(|i| words.words()[i] as _) }
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────

pub fn opt_lookup(name: &str) -> Option<&'static Opt> {
    OPTS.iter().find(|o| o.name == name)
}

/// As a mount option names it: "quota" is usrquota.
fn mount_opt_lookup(name: &str) -> Option<&'static Opt> {
    opt_lookup(match name {
        "quota" => "usrquota",
        n       => n,
    })
}

/// Values mount options spell differently: degraded's bools.
fn opt_val_synonym<'a>(name: &str, val: &'a [u8]) -> &'a [u8] {
    match (name, val) {
        ("degraded", b"true" | b"1")  => b"yes",
        ("degraded", b"false" | b"0") => b"no",
        _ => val,
    }
}

/// @opt's name and @msg, into @err if it's given.
fn complain(err: Option<&mut Printbuf>, opt: &Opt, msg: core::fmt::Arguments<'_>) {
    if let Some(err) = err {
        write!(err, "{}: {msg}", opt.name);
    }
}

/// Whether @v is within @opt's bounds, and what its flags and type require.
pub fn opt_validate(opt: &Opt, v: u64, mut err: Option<&mut Printbuf>) -> Result<(), BchError> {
    macro_rules! fail {
        ($e:expr, $($msg:tt)*) => {{
            complain(err.as_deref_mut(), opt, format_args!($($msg)*));
            return Err($e.into());
        }};
    }

    if v < opt.min {
        fail!(Errcode::BCH_ERR_ERANGE_option_too_small, "too small (min {})", opt.min);
    }
    if opt.max != 0 && v > opt.max {
        fail!(Errcode::BCH_ERR_ERANGE_option_too_big, "too big (max {})", opt.max);
    }
    if opt.has(c::OptFlags::OPT_SB_FIELD_SECTORS) && v & 511 != 0 {
        fail!(Errcode::BCH_ERR_opt_parse_error, "not a multiple of 512");
    }
    if opt.has(c::OptFlags::OPT_MUST_BE_POW_2) && !v.is_power_of_two() {
        fail!(Errcode::BCH_ERR_opt_parse_error, "must be a power of two");
    }
    match opt.ty {
        OptType::Fn(OptFn { validate: Some(validate), .. }) => validate(v, err),
        _ => Ok(()),
    }
}

/// A bitfield option's value: the choices named, comma or semicolon
/// separated - or with a leading '-', those allowed but not named.
/// Err(a name that isn't one of the choices, or isn't allowed).
pub fn read_flag_list<'a>(val: &'a [u8], choices: &[&str], allowed: u64) -> Result<u64, &'a [u8]> {
    let s = val.trim_ascii();
    let (invert, s) = match s.strip_prefix(b"-") {
        Some(s) => (true, s),
        None    => (false, s),
    };

    let mut v = 0;
    for name in s.split(|&b| b == b',' || b == b';') {
        let bit = choices.iter().position(|c| c.as_bytes() == name).ok_or(name)?;
        if allowed & 1 << bit == 0 {
            return Err(name);
        }
        v |= 1 << bit;
    }

    if invert {
        let all = if choices.len() >= 64 { !0 } else { (1 << choices.len()) - 1 };
        v = !v & all & allowed;
    }
    Ok(v)
}

/// @val as an @opt value: @fs enables options that need one.
pub fn opt_parse(fs: Option<&Fs>, opt: &Opt, val: Option<&[u8]>, mut err: Option<&mut Printbuf>)
    -> Result<u64, BchError>
{
    if let Some(err) = err.as_deref_mut() {
        err.indent_add_nextline(2);
    }
    macro_rules! fail {
        ($e:expr, $($msg:tt)*) => {{
            complain(err.as_deref_mut(), opt, format_args!($($msg)*));
            return Err($e.into());
        }};
    }

    let v = match opt.ty {
        OptType::Bool => match val.unwrap_or(b"1") {
            b"1" | b"true" | b"yes" => 1,
            b"0" | b"false" | b"no" => 0,
            _ => fail!(Errcode::BCH_ERR_option_not_bool, "must be bool"),
        },
        OptType::Uint => {
            let Some(val) = val else {
                fail!(Errcode::BCH_ERR_EINVAL_opt_parse_uint_required, "required value");
            };
            if val.starts_with(b"-") {
                fail!(Errcode::BCH_ERR_option_negative, "must be a non-negative number");
            }
            let v = core::str::from_utf8(val)
                .map_err(|_| BchError::from(c::EINVAL))
                .and_then(|s| if opt.has(c::OptFlags::OPT_HUMAN_READABLE) {
                    parse_human_u64(s)
                } else {
                    parse_int(s)
                });
            match v {
                Ok(v)  => v,
                Err(e) => fail!(e, "must be a number"),
            }
        }
        OptType::Str(choices) => {
            let Some(val) = val else {
                fail!(Errcode::BCH_ERR_EINVAL_opt_parse_str_required, "required value");
            };
            match choices.iter().position(|c| c.as_bytes() == val) {
                Some(i) => i as u64,
                None    => fail!(c::EINVAL, "invalid selection"),
            }
        }
        OptType::Bitfield { choices, allowed } => {
            let Some(val) = val else {
                fail!(Errcode::BCH_ERR_EINVAL_opt_parse_str_required, "required value");
            };
            match read_flag_list(val, choices, allowed) {
                Ok(v)    => v,
                Err(bad) => fail!(Errcode::BCH_ERR_opt_parse_error, "invalid flag {}",
                                  core::str::from_utf8(bad).unwrap_or("(not UTF-8)")),
            }
        }
        OptType::StrMember { size, .. } => {
            let Some(val) = val else {
                fail!(Errcode::BCH_ERR_EINVAL_opt_parse_str_required, "required value");
            };
            if val.len() > size {
                fail!(c::ENAMETOOLONG, "too long (max {size} bytes)");
            }
            // Set in the member directly, by opt_set_sb()
            0
        }
        OptType::Fn(f) => match (f.parse)(fs, val, err.as_deref_mut()) {
            Ok(v) => v,
            Err(e) if e == Errcode::BCH_ERR_option_needs_open_fs.into() => return Err(e),
            Err(e) => fail!(e, "parse error"),
        },
    };

    opt_validate(opt, v, err)?;
    Ok(v)
}

/// One mount option, @name=@val, into @opts if it's given: an unknown
/// option is skipped, and one that can't be parsed before the filesystem is
/// open goes into @parse_later, if it's given, to parse then.
pub fn parse_one_mount_opt(fs: Option<&Fs>, opts: Option<&mut c::bch_opts>,
                           parse_later: Option<&mut Printbuf>, name: &[u8], val: Option<&[u8]>,
                           err: &mut Printbuf) -> Result<(), BchError> {
    let name_str = core::str::from_utf8(name).ok();
    let mut opt = name_str.and_then(mount_opt_lookup);
    let mut val = val;

    // "noopt", a bool's negation
    if opt.is_none() && val.is_none() {
        if let Some(n) = name_str.and_then(|n| n.strip_prefix("no")) {
            opt = mount_opt_lookup(n);
            val = Some(b"0");
        }
    }

    let Some(opt) = opt else {
        return Ok(());
    };

    // a value for the synonyms - but OPT_FN is weird
    if val.is_none() && !matches!(opt.ty, OptType::Fn(_)) {
        val = Some(b"1");
    }
    let val = val.map(|v| opt_val_synonym(name_str.unwrap_or_default(), v));

    if !opt.has(c::OptFlags::OPT_MOUNT | c::OptFlags::OPT_MOUNT_OLD) {
        return Err(Errcode::BCH_ERR_option_name.into());
    }

    if (opt.id == OptId::Opt_usrquota || opt.id == OptId::Opt_grpquota) && !cfg!(CONFIG_BCACHEFS_QUOTA) {
        return Err(Errcode::BCH_ERR_option_name.into());
    }

    let v = match opt_parse(fs, opt, val, Some(err)) {
        Err(e) if e == Errcode::BCH_ERR_option_needs_open_fs.into() => {
            if let Some(later) = parse_later {
                later.write_bytes(name);
                write!(later, "=");
                later.write_bytes(val.unwrap_or_default());
                write!(later, ",");
                if later.allocation_failure() {
                    return Err(c::ENOMEM.into());
                }
            }
            return Ok(());
        }
        r => r?,
    };

    if opt.has(c::OptFlags::OPT_MOUNT_OLD) {
        log_no_fs(LogLevel::ERR, crate::message!(
            "option {} may no longer be specified at mount time; set via sysfs opts dir", opt.name));
        return Ok(());
    }

    if let Some(opts) = opts {
        opts.set(opt.id, v);
    }
    Ok(())
}

/// A mount option string, comma separated, into @opts: what can only be
/// parsed with the filesystem open into @parse_later, as
/// parse_one_mount_opt().
pub fn parse_mount_opts_into(fs: Option<&Fs>, opts: &mut c::bch_opts, mut parse_later: Option<&mut Printbuf>,
                             options: &[u8], ignore_unknown: bool) -> Result<(), BchError> {
    // sys_fsconfig() occasionally hands us a list starting with a comma
    let options = options.strip_prefix(b",").unwrap_or(options);

    for opt in options.split(|&b| b == b',').filter(|o| !o.is_empty()) {
        let (name, val) = match opt.iter().position(|&b| b == b'=') {
            Some(i) => (&opt[..i], Some(&opt[i + 1..])),
            None    => (opt, None),
        };

        let mut err = Printbuf::new();
        match parse_one_mount_opt(fs, Some(opts), parse_later.as_deref_mut(), name, val, &mut err) {
            Err(e) if e == Errcode::BCH_ERR_option_name.into() && ignore_unknown => {}
            Err(e) => {
                log_no_fs(LogLevel::ERR, crate::message!("Error parsing option {}", err.as_str()));
                return Err(e);
            }
            Ok(()) => {}
        }
    }
    Ok(())
}

/// @optstr parsed into fresh options: what can only be parsed on a running
/// filesystem too, if @fs is given.
pub fn parse_mount_opts_cstr(fs: Option<&Fs>, optstr: &CStr, ignore_unknown: bool)
        -> Result<c::bch_opts, BchError> {
    let mut opts = Default::default();
    parse_mount_opts_into(fs, &mut opts, None, optstr.to_bytes(), ignore_unknown)?;
    Ok(opts)
}

pub fn parse_mount_opts(fs: Option<&Fs>, optstr: Option<&str>, ignore_unknown: bool)
        -> Result<c::bch_opts, BchError> {
    let mut opts = Default::default();
    if let Some(s) = optstr {
        parse_mount_opts_into(fs, &mut opts, None, s.as_bytes(), ignore_unknown)?;
    }
    Ok(opts)
}

#[cfg(feature = "std")]
pub fn parse_mount_opts_vec(opts: &[String], ignore_unknown: bool) -> Result<c::bch_opts, BchError> {
    parse_mount_opts(None, Some(&opts.join(",")), ignore_unknown)
}

// ── Printing ─────────────────────────────────────────────────────────────

/// @flags: OPT_SHOW_*. Against @fs, or without one @sb: a target's, a
/// label's names are theirs.
#[cold]
pub fn opt_to_text(out: &mut Printbuf, fs: Option<&Fs>, sb: Option<&c::bch_sb<Unvalidated>>, opt: &Opt, v: u64,
                   flags: u32) {
    if flags & c::OPT_SHOW_MOUNT_STYLE != 0 {
        if let OptType::Bool = opt.ty {
            write!(out, "{}{}", if v != 0 { "" } else { "no" }, opt.name);
            return;
        }
        write!(out, "{}=", opt.name);
    }

    match opt.ty {
        OptType::Bool | OptType::Uint if opt.has(c::OptFlags::OPT_HUMAN_READABLE) => prt!(out, "{}", HumanReadable(v)),
        OptType::Bool | OptType::Uint => write!(out, "{}", v as i64),
        OptType::Str(choices) => match choices.get(v as usize) {
            Some(_) if v < opt.min || v > opt.max => write!(out, "(invalid option {})", v as i64),
            Some(_) if flags & c::OPT_SHOW_FULL_LIST != 0 => {
                for (i, c) in choices.iter().enumerate() {
                    if i as u64 == v {
                        write!(out, "[{c}] ");
                    } else {
                        write!(out, "{c} ");
                    }
                }
            }
            Some(c) => write!(out, "{c}"),
            None    => write!(out, "(invalid option {})", v as i64),
        },
        OptType::Bitfield { choices, .. } => out.bitflags(choices, v),
        OptType::Fn(f) => (f.to_text)(out, fs, sb, v),
        // Shown per device, from the member: bch2_member_to_text()
        OptType::StrMember { .. } => {}
    }
}

/// The options in @opts that aren't the defaults, comma separated: those
/// flagged with any of @show_mask and none of @hide_mask, and of @mask if
/// it's given.
#[cold]
pub fn opts_to_text(out: &mut Printbuf, opts: &c::bch_opts, fs: Option<&Fs>, sb: Option<&c::bch_sb<Unvalidated>>,
                    mask: Option<&c::bch_opts_mask>, show_mask: u32, hide_mask: u32, flags: u32) {
    let mut first = true;

    for opt in &OPTS {
        let f = opt.flags.bits();
        if f & hide_mask != 0 || f & show_mask == 0 {
            continue;
        }
        if mask.is_some_and(|m| !m.test(opt.id)) {
            continue;
        }

        let v = opts.get(opt.id);
        if v == DEFAULTS.get(opt.id) {
            continue;
        }

        if !first {
            write!(out, ",");
        }
        first = false;

        opt_to_text(out, fs, sb, opt, v, flags);
    }
}

// ── The superblock ───────────────────────────────────────────────────────

fn warn_no_member(opt: &Opt, dev: u32) {
    log_no_fs(LogLevel::WARN, crate::message!(
        "tried to set device option {} on nonexistent device {dev}", opt.name));
    crate::warn_on!(true);
}

/// Option @id as the superblock has it - a device option, as member @dev's:
/// 0 if it doesn't.
pub fn opt_from_sb(sb: &c::bch_sb<Unvalidated>, id: OptId, dev: Option<u32>) -> u64 {
    let opt = id.opt();
    let v = match dev {
        None => opt.fs_field().and_then(|f| f.get(sb, 0)).unwrap_or(0),
        Some(dev) => match opt.member_field().and_then(|f| f.get(sb, dev)) {
            Some(v) => v,
            None => {
                warn_no_member(opt, dev);
                return 0;
            }
        },
    };
    opt.from_sb_field(v)
}

/// The filesystem defaults @sb has, into @opts - every one it has,
/// defined.
pub fn opts_from_sb(opts: &mut c::bch_opts, sb: &c::bch_sb<Unvalidated>) {
    for opt in OPTS.iter().filter(|o| o.fs_field().is_some()) {
        opts.set(opt.id, opt_from_sb(sb, opt.id, None));
    }
}

/// Whether the filesystem defaults @sb has are valid, as options: an
/// invalid one said in @out.
pub fn opts_validate_sb(sb: &c::bch_sb<Unvalidated>, out: &mut Printbuf) -> Result<(), BchError> {
    for opt in OPTS.iter().filter(|o| o.fs_field().is_some()) {
        let mut err = Printbuf::new();
        if let Err(e) = opt_validate(opt, opt_from_sb(sb, opt.id, None), Some(&mut err)) {
            writeln!(out, "Invalid superblock option {}", err.as_str());
            return Err(e);
        }
    }
    Ok(())
}

/// A filesystem's options: @sb's, valid, and the mount's @opts over them
pub fn fs_opts(sb: &c::bch_sb<Unvalidated>, opts: &c::bch_opts, out: &mut Printbuf) -> Result<c::bch_opts, BchError> {
    opts_validate_sb(sb, out)?;

    let mut fs_opts = DEFAULTS;
    opts_from_sb(&mut fs_opts, sb);
    fs_opts.apply(opts);

    #[cfg(all(kernel, not(CONFIG_TRANSPARENT_HUGEPAGE)))]
    if fs_opts.block_size as usize > crate::os::kernel::PAGE_SIZE {
        prt!(out, "cannot mount bs > ps filesystem without CONFIG_TRANSPARENT_HUGEPAGE\n");
        return Err(Errcode::BCH_ERR_EINVAL_block_size_needs_thp.into());
    }
    Ok(fs_opts)
}

/// Set @opt in @sb, to @v - for a device option in member @dev's - @val its
/// string for a member string option: whether it changed. Err if @v doesn't
/// fit in its field: options with no max have only the field's.
pub fn opt_set_sb(sb: &mut c::bch_sb_buf, dev: Option<u32>, opt: &Opt, v: u64, val: Option<&[u8]>)
    -> Result<bool, BchError>
{
    let v = opt.to_sb_field(v);
    let mut changed = false;

    if let (true, None, Some(f)) = (opt.has(c::OptFlags::OPT_FS), dev, opt.fs_field()) {
        changed = f.set(sb, 0, v)?.unwrap_or(false);
    }

    if let (true, Some(dev), Some(f)) = (opt.has(c::OptFlags::OPT_DEVICE), dev, opt.member_field()) {
        let Some(c) = f.set(sb, dev, v)? else {
            warn_no_member(opt, dev);
            return Ok(false);
        };
        changed = c;

        if opt.id == OptId::Opt_rotational {
            let m = sb.member_mut(dev).expect("set above");
            changed |= !m.rotational_set();
            m.set_rotational_set(true);
        }
    }

    if let (OptType::StrMember { get_mut, .. }, true, Some(dev)) = (&opt.ty, opt.has(c::OptFlags::OPT_DEVICE), dev) {
        let m = members::sb_member(sb.sb(), dev).filter(|m| m.alive()).and_then(|_| sb.member_mut(dev));
        let Some(m) = m else {
            warn_no_member(opt, dev);
            return Ok(false);
        };
        let val = val.unwrap_or_default();
        // zero padded, so equal strings compare equal in the interning
        for (i, b) in get_mut(m).iter_mut().enumerate() {
            let want = val.get(i).copied().unwrap_or(0);
            changed |= *b != want;
            *b = want;
        }
    }

    Ok(changed)
}

impl Fs {
    /// Set @opt in the superblock - in device @ca's member for a device
    /// option - @val its string for a member string option, and write it if
    /// that changed it: whether it did.
    pub fn opt_set_sb(&self, ca: Option<&Dev>, opt: &Opt, v: u64, val: Option<&[u8]>) -> Result<bool, BchError> {
        let mut sb = self.sb_lock();
        let changed = opt_set_sb(&mut sb, ca.map(|ca| ca.dev_idx.into()), opt, v, val)?;
        if changed {
            self.write_super(&mut sb);
        }
        Ok(changed)
    }
}

mod change;
pub use change::OptChange;
pub(crate) use change::opts_hooks_pre_set;
