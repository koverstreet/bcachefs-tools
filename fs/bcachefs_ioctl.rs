// SPDX-License-Identifier: GPL-2.0

//! The data types of bcachefs_ioctl.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_const, c_enum, c_ioctl, c_xmacro, CStruct};
use nestify::nest;
use typeinfo_macros::TypeInfo;

c_const! {
    /*
     * Flags common to multiple ioctls:
     */
    #[c_int]
    pub const BCH_FORCE_IF_DATA_LOST: u32 = 1 << 0;
}

c_const! {
    #[c_int]
    pub const BCH_FORCE_IF_METADATA_LOST: u32 = 1 << 1;
}

c_const! {
    #[c_int]
    pub const BCH_FORCE_IF_DATA_DEGRADED: u32 = 1 << 2;
}

c_const! {
    #[c_int]
    pub const BCH_FORCE_IF_METADATA_DEGRADED: u32 = 1 << 3;
}

c_const! {
    #[c_int]
    pub const BCH_FORCE_IF_LOST: u32 =
        c::BCH_FORCE_IF_DATA_LOST |
        c::BCH_FORCE_IF_METADATA_LOST;
}

c_const! {
    #[c_int]
    pub const BCH_FORCE_IF_DEGRADED: u32 =
        c::BCH_FORCE_IF_DATA_DEGRADED |
        c::BCH_FORCE_IF_METADATA_DEGRADED;
}

c_const! {
    /*
     * If cleared, ioctl that refer to a device pass it as a pointer to a pathname
     * (e.g. /dev/sda1); if set, the dev field is the device's index within the
     * filesystem:
     */
    #[c_int]
    pub const BCH_BY_INDEX: u32 = 1 << 4;
}

c_const! {
    /*
     * For BCH_IOCTL_READ_SUPER: get superblock of a specific device, not filesystem
     * wide superblock:
     */
    #[c_int]
    pub const BCH_READ_DEV: u32 = 1 << 5;
}

c_ioctl! {
    /* filesystem ioctls: */

    BCH_IOCTL_QUERY_UUID		= _IOR(0xbc,	1,  c::bch_ioctl_query_uuid),

    BCH_IOCTL_DISK_ADD			= _IOW(0xbc,	4,  c::bch_ioctl_disk),
    BCH_IOCTL_DISK_ADD_v2		= _IOW(0xbc,	23, c::bch_ioctl_disk_v2),
    BCH_IOCTL_DISK_REMOVE		= _IOW(0xbc,	5,  c::bch_ioctl_disk),
    BCH_IOCTL_DISK_REMOVE_v2		= _IOW(0xbc,	24, c::bch_ioctl_disk_v2),
    BCH_IOCTL_DISK_ONLINE		= _IOW(0xbc,	6,  c::bch_ioctl_disk),
    BCH_IOCTL_DISK_ONLINE_v2		= _IOW(0xbc,	25, c::bch_ioctl_disk_v2),
    BCH_IOCTL_DISK_OFFLINE		= _IOW(0xbc,	7,  c::bch_ioctl_disk),
    BCH_IOCTL_DISK_OFFLINE_v2		= _IOW(0xbc,	26, c::bch_ioctl_disk_v2),
    BCH_IOCTL_DISK_SET_STATE		= _IOW(0xbc,	8,  c::bch_ioctl_disk_set_state),
    BCH_IOCTL_DISK_SET_STATE_v2		= _IOW(0xbc,	22, c::bch_ioctl_disk_set_state_v2),
    BCH_IOCTL_DATA			= _IOW(0xbc,	10, c::bch_ioctl_data),
    BCH_IOCTL_FS_USAGE			= _IOWR(0xbc,	11, c::bch_ioctl_fs_usage),
    BCH_IOCTL_DEV_USAGE			= _IOWR(0xbc,	11, c::bch_ioctl_dev_usage),
    BCH_IOCTL_READ_SUPER		= _IOW(0xbc,	12, c::bch_ioctl_read_super),
    BCH_IOCTL_DISK_GET_IDX		= _IOW(0xbc,	13, c::bch_ioctl_disk_get_idx),
    BCH_IOCTL_DISK_RESIZE		= _IOW(0xbc,	14, c::bch_ioctl_disk_resize),
    BCH_IOCTL_DISK_RESIZE_v2		= _IOW(0xbc,	27, c::bch_ioctl_disk_resize_v2),
    BCH_IOCTL_DISK_RESIZE_JOURNAL	= _IOW(0xbc,	15, c::bch_ioctl_disk_resize_journal),
    BCH_IOCTL_DISK_RESIZE_JOURNAL_v2	= _IOW(0xbc,	28, c::bch_ioctl_disk_resize_journal_v2),

    BCH_IOCTL_SUBVOLUME_CREATE		= _IOW(0xbc,	16, c::bch_ioctl_subvolume),
    BCH_IOCTL_SUBVOLUME_CREATE_v2	= _IOW(0xbc,	29, c::bch_ioctl_subvolume_v2),
    BCH_IOCTL_SUBVOLUME_DESTROY		= _IOW(0xbc,	17, c::bch_ioctl_subvolume),
    BCH_IOCTL_SUBVOLUME_DESTROY_v2	= _IOW(0xbc,	30, c::bch_ioctl_subvolume_v2),

    BCH_IOCTL_DEV_USAGE_V2		= _IOWR(0xbc,	18, c::bch_ioctl_dev_usage_v2),

    BCH_IOCTL_FSCK_OFFLINE		= _IOW(0xbc,	19, c::bch_ioctl_fsck_offline),
    BCH_IOCTL_FSCK_ONLINE		= _IOW(0xbc,	20, c::bch_ioctl_fsck_online),
    BCH_IOCTL_QUERY_ACCOUNTING		= _IOW(0xbc,	21, c::bch_ioctl_query_accounting),
    BCH_IOCTL_QUERY_COUNTERS		= _IOW(0xbc,	21, c::bch_ioctl_query_counters),
    BCH_IOCTL_SUBVOLUME_LIST		= _IOWR(0xbc,	31, c::bch_ioctl_subvol_readdir),
    BCH_IOCTL_SUBVOLUME_TO_PATH		= _IOWR(0xbc,	32, c::bch_ioctl_subvol_to_path),
    BCH_IOCTL_SNAPSHOT_TREE		= _IOWR(0xbc,	33, c::bch_ioctl_snapshot_tree_query),
    BCH_IOCTL_QUERY_BTREE_KEYS		= _IOWR(0xbc,	34, c::bch_ioctl_query_btree_keys),
    BCH_IOCTL_SNAPSHOT_TREE_v2		= _IOWR(0xbc,	35, c::bch_ioctl_snapshot_tree_query_v2),
    BCH_IOCTL_RECOVERY_STATUS		= _IOR(0xbc,	36, c::bch_ioctl_recovery_status),
    BCH_IOCTL_QUERY_ACCOUNTING_v2	= _IOW(0xbc,	37, c::bch_ioctl_query_accounting_v2),
}

c_ioctl! {
    /* ioctl below act on a particular file, not the filesystem as a whole: */

    #[c("const char __user *")]
    BCHFS_IOC_REINHERIT_ATTRS		= _IOR(0xbc, 64, *const crate::util::ffi::c_char),
    BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS = _IO(0xbc, 65),
    BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS	= _IO(0xbc, 66),
    BCHFS_IOC_PREAD_RAW			= _IOWR(0xbc, 67, c::bch_ioctl_pread_raw),
    BCHFS_IOC_UNPOISON			= _IOW(0xbc, 68, c::bch_ioctl_unpoison),
    BCHFS_IOC_GET_DAMAGE		= _IOWR(0xbc, 70, c::bch_ioctl_get_damage),
    BCHFS_IOC_READDIR_FLAGS		= _IOWR(0xbc, 69, c::bch_ioctl_readdir_flags),

    /*
     * BCHFS_IOC_CLEAR_DAMAGE: clear the file's damage record, in the calling
     * subvolume's view - snapshots keep theirs (on disk the clear is a
     * whiteout when an older version still needs the record). Requires
     * ownership or CAP_FOWNER, like chattr.
     */
    BCHFS_IOC_CLEAR_DAMAGE		= _IO(0xbc, 71),
}

/*
 * BCHFS_IOC_GET_DAMAGE: the accumulated damage record for this file - the
 * union of damage recorded against its inode in the file's snapshot and
 * all ancestor snapshot versions, since damage done to an ancestor version
 * is damage to the file seen here.
 *
 * @nr_entries	- in: capacity of @entries; out: number present. A result
 *		  exceeding the capacity reports the true count - retry
 *		  with more room.
 * @entries	- sorted by error id; the same records the errors
 *		  superblock section keeps (bch_sb_field_error_entry_v2):
 *		  the id, a saturating occurrence count and the times of
 *		  first and last occurrence, unpacked with
 *		  BCH_SB_ERROR_ENTRY_V2_ID/NR/FIRST/LAST
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_get_damage {
    pub nr_entries: u32,
    pub pad: u32,
    #[c("bch_sb_field_error_entry_v2 entries[]")]
    pub entries: [c::bch_sb_field_error_entry_v2; 0],
}

c_const! {
    /*
     * BCHFS_IOC_READDIR_FLAGS: readdir with filters, on the directory the
     * ioctl is called on.
     *
     * recursive: entries from the whole subtree, names become paths relative
     * to the fd's directory. The filters pick the iteration: damaged
     * walks the damage btree (cost proportional to recorded damage, not tree
     * size), subvolumes_only walks the subvolume tree, an unfiltered
     * recursive listing is an honest tree walk.
     *
     * damaged: only entries whose inode has recorded damage, in its
     * snapshot version or an ancestor.
     *
     * Permissions are those of readdir: the caller learns nothing beyond the
     * directory they opened.
     *
     * @flags	- BCH_READDIR_*
     * @pos		- opaque resume cursor: zero to start; copied back out
     *		  past the last entry returned. Iterate until @used == 0.
     * @buf_size,
     * @buf		- userspace buffer, filled with struct
     *		  bch_ioctl_readdir_entry records
     * @used	- out: bytes of @buf filled
     */
    pub const BCH_READDIR_recursive: u32 = 1 << 0;
}

c_const! {
    pub const BCH_READDIR_damaged: u32 = 1 << 2;
}

c_const! {
    pub const BCH_READDIR_subvolumes_only: u32 = 1 << 1;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_readdir_flags {
    pub pos: [u64; 2],
    pub buf: u64,
    pub buf_size: u32,
    pub flags: u32,
    pub used: u32,
    pub pad: u32,
}

/*
 * One entry: the NUL-terminated name - a relative path, under recursive -
 * follows the fixed header; entries are padded to 8-byte alignment.
 * Deliberately knows nothing about the filters that selected it: damage
 * details come from BCHFS_IOC_GET_DAMAGE on the file itself.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_readdir_entry {
    pub inum: u64,
    pub d_type: u8,
    pub pad: u8,
    pub name_len: u16, /* including the NUL */
    pub name: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_err_msg {
    pub msg_ptr: u64,
    pub msg_len: u32,
    pub pad: u32,
}

/*
 * BCH_IOCTL_QUERY_UUID: get filesystem UUID
 *
 * Returns user visible UUID, not internal UUID (which may not ever be changed);
 * the filesystem's sysfs directory may be found under /sys/fs/bcachefs with
 * this UUID.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_ioctl_query_uuid {
    pub uuid: c::__uuid_t,
}
c_default!(bch_ioctl_query_uuid);

/*
 * BCH_IOCTL_DISK_ADD: add a new device to an existing filesystem
 *
 * The specified device must not be open or in use. On success, the new device
 * will be an online member of the filesystem just like any other member.
 *
 * The device must first be prepared by userspace by formatting with a bcachefs
 * superblock, which is only used for passing in superblock options/parameters
 * for that device (in struct bch_member). The new device's superblock should
 * not claim to be a member of any existing filesystem - UUIDs on it will be
 * ignored.
 */

/*
 * BCH_IOCTL_DISK_REMOVE: permanently remove a member device from a filesystem
 *
 * Any data present on @dev will be permanently deleted, and @dev will be
 * removed from its slot in the filesystem's list of member devices. The device
 * may be either offline or offline.
 *
 * Will fail removing @dev would leave us with insufficient read write devices
 * or degraded/unavailable data, unless the approprate BCH_FORCE_IF_* flags are
 * set.
 */

/*
 * BCH_IOCTL_DISK_ONLINE: given a disk that is already a member of a filesystem
 * but is not open (e.g. because we started in degraded mode), bring it online
 *
 * all existing data on @dev will be available once the device is online,
 * exactly as if @dev was present when the filesystem was first mounted
 */

/*
 * BCH_IOCTL_DISK_OFFLINE: offline a disk, causing the kernel to close that
 * block device, without removing it from the filesystem (so it can be brought
 * back online later)
 *
 * Data present on @dev will be unavailable while @dev is offline (unless
 * replicated), but will still be intact and untouched if @dev is brought back
 * online
 *
 * Will fail (similarly to BCH_IOCTL_DISK_SET_STATE) if offlining @dev would
 * leave us with insufficient read write devices or degraded/unavailable data,
 * unless the approprate BCH_FORCE_IF_* flags are set.
 */

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk {
    pub flags: u32,
    pub pad: u32,
    pub dev: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk_v2 {
    pub flags: u32,
    pub pad: u32,
    pub dev: u64,
    pub err: c::bch_ioctl_err_msg,
}

/*
 * BCH_IOCTL_DISK_SET_STATE: modify state of a member device of a filesystem
 *
 * @new_state		- one of the bch_member_state states (rw, ro, failed,
 *			  spare)
 *
 * Will refuse to change member state if we would then have insufficient devices
 * to write to, or if it would result in degraded data (when @new_state is
 * failed or spare) unless the appropriate BCH_FORCE_IF_* flags are set.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk_set_state {
    pub flags: u32,
    pub new_state: u8,
    pub pad: [u8; 3],
    pub dev: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk_set_state_v2 {
    pub flags: u32,
    pub new_state: u8,
    pub pad: [u8; 3],
    pub dev: u64,
    pub err: c::bch_ioctl_err_msg,
}

c_xmacro! {
    BCH_DATA_OPS(x) {
        (scrub, 0),
        (rereplicate, 1),
        (migrate, 2),
        (rewrite_old_nodes, 3),
        (drop_extra_replicas, 4),
    }
}

macro_rules! __bch_data_ops_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_data_ops: u32 {
                $($acc)*
                $([<BCH_DATA_OP_ $t>] = (($n) as u32),)*
                BCH_DATA_OP_NR,
            }
        }
    } };
}
BCH_DATA_OPS!(__bch_data_ops_0 []);

/*
 * BCH_IOCTL_DATA: operations that walk and manipulate filesystem data (e.g.
 * scrub, rereplicate, migrate).
 *
 * This ioctl kicks off a job in the background, and returns a file descriptor.
 * Reading from the file descriptor returns a struct bch_ioctl_data_event,
 * indicating current progress, and closing the file descriptor will stop the
 * job. The file descriptor is O_CLOEXEC.
 */
nest! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[repr(C, align(8))]
    #[c_packed]
    pub struct bch_ioctl_data {
        pub op: u16,
        pub start_btree: u8,
        pub end_btree: u8,
        pub flags: u32,

        pub start_pos: c::bpos,
        pub end_pos: c::bpos,

        #[c_anon]
        #>[repr(C)]
        pub args: pub union bch_ioctl_data_args {
            #[c_inline]
            #>[repr(C)]
            pub scrub: pub struct bch_ioctl_data_scrub {
                pub dev: u32,
                pub data_types: u32,
            },
            #[c_inline]
            #>[repr(C)]
            pub migrate: pub struct bch_ioctl_data_migrate {
                pub dev: u32,
                pub pad: u32,
            },
            #[c_anon]
            #>[repr(C)]
            pub __pad: pub struct bch_ioctl_data_pad {
                pub pad: [u64; 8],
            },
        },
    }
}
c_default!(bch_ioctl_data);

c_enum! {
    #[open]
    pub enum bch_data_event: u32 {
        BCH_DATA_EVENT_PROGRESS = 0,
        /* XXX: add an event for reporting errors */
        BCH_DATA_EVENT_NR = 1,
    }
}

c_enum! {
    #[open]
    pub enum data_progress_data_type_special: u32 {
        DATA_PROGRESS_DATA_TYPE_phys = 254,
        DATA_PROGRESS_DATA_TYPE_done = 255,
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_ioctl_data_progress {
    pub data_type: u8,
    pub btree_id: u8,
    pub pad: [u8; 2],
    pub pos: c::bpos,

    pub sectors_done: u64,
    pub sectors_total: u64,
    pub sectors_error_corrected: u64,
    pub sectors_error_uncorrected: u64,
}

c_enum! {
    #[open]
    pub enum bch_ioctl_data_event_ret: u32 {
        BCH_IOCTL_DATA_EVENT_RET_done = 1,
        BCH_IOCTL_DATA_EVENT_RET_device_offline = 2,
    }
}

nest! {
    #[derive(Clone, Copy, CStruct, TypeInfo)]*
    #[repr(C, align(8))]
    pub struct bch_ioctl_data_event {
        pub type_: u8,
        pub ret: u8,
        pub pad: [u8; 6],
        #[c_anon]
        #>[repr(C)]
        pub payload: pub union bch_ioctl_data_event_payload {
            pub p: c::bch_ioctl_data_progress,
            pub pad2: [u64; 15],
        },
    }
}
c_default!(bch_ioctl_data_event);

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_replicas_usage {
    pub sectors: u64,
    pub r: c::bch_replicas_entry_v1,
}

/* Obsolete */
/*
 * BCH_IOCTL_FS_USAGE: query filesystem disk space usage
 *
 * Returns disk space usage broken out by data type, number of replicas, and
 * by component device
 *
 * @replica_entries_bytes - size, in bytes, allocated for replica usage entries
 *
 * On success, @replica_entries_bytes will be changed to indicate the number of
 * bytes actually used.
 *
 * Returns -ERANGE if @replica_entries_bytes was too small
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_fs_usage {
    pub capacity: u64,
    pub used: u64,
    pub online_reserved: u64,
    pub persistent_reserved: [u64; c::BCH_REPLICAS_MAX as usize],

    pub replica_entries_bytes: u32,
    pub pad: u32,

    pub replicas: [c::bch_replicas_usage; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_dev_usage_type {
    pub buckets: u64,
    pub sectors: u64,
    pub fragmented: u64,
}

/* Obsolete */
/*
 * BCH_IOCTL_DEV_USAGE: query device disk space usage
 *
 * Returns disk space usage broken out by data type - both by buckets and
 * sectors.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_dev_usage {
    pub dev: u64,
    pub flags: u32,
    pub state: u8,
    pub pad: [u8; 7],

    pub bucket_size: u32,
    pub nr_buckets: u64,

    pub buckets_ec: u64,

    pub d: [c::bch_ioctl_dev_usage_type; 10],
}

/* Obsolete */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_dev_usage_v2 {
    pub dev: u64,
    pub flags: u32,
    pub state: u8,
    pub nr_data_types: u8,
    pub pad: [u8; 6],

    pub bucket_size: u32,
    pub nr_buckets: u64,

    pub d: [c::bch_ioctl_dev_usage_type; 0],
}

/*
 * BCH_IOCTL_READ_SUPER: read filesystem superblock
 *
 * Equivalent to reading the superblock directly from the block device, except
 * avoids racing with the kernel writing the superblock or having to figure out
 * which block device to read
 *
 * @sb		- buffer to read into
 * @size	- size of userspace allocated buffer
 * @dev		- device to read superblock for, if BCH_READ_DEV flag is
 *		  specified
 *
 * Returns -ERANGE if buffer provided is too small
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_read_super {
    pub flags: u32,
    pub pad: u32,
    pub dev: u64,
    pub size: u64,
    pub sb: u64,
}

/*
 * BCH_IOCTL_DISK_GET_IDX: give a path to a block device, query filesystem to
 * determine if disk is a (online) member - if so, returns device's index
 *
 * Returns -ENOENT if not found
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk_get_idx {
    pub dev: u64,
}

/*
 * BCH_IOCTL_DISK_RESIZE: resize filesystem on a device
 *
 * @dev		- member to resize
 * @nbuckets	- new number of buckets
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk_resize {
    pub flags: u32,
    pub pad: u32,
    pub dev: u64,
    pub nbuckets: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk_resize_v2 {
    pub flags: u32,
    pub pad: u32,
    pub dev: u64,
    pub nbuckets: u64,
    pub err: c::bch_ioctl_err_msg,
}

/*
 * BCH_IOCTL_DISK_RESIZE_JOURNAL: resize journal on a device
 *
 * @dev		- member to resize
 * @nbuckets	- new number of buckets
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk_resize_journal {
    pub flags: u32,
    pub pad: u32,
    pub dev: u64,
    pub nbuckets: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_disk_resize_journal_v2 {
    pub flags: u32,
    pub pad: u32,
    pub dev: u64,
    pub nbuckets: u64,
    pub err: c::bch_ioctl_err_msg,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_subvolume {
    pub flags: u32,
    pub dirfd: u32,
    pub mode: u16,
    pub pad: [u16; 3],
    pub dst_ptr: u64,
    pub src_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_subvolume_v2 {
    pub flags: u32,
    pub dirfd: u32,
    pub mode: u16,
    pub pad: [u16; 3],
    pub dst_ptr: u64,
    pub src_ptr: u64,
    pub err: c::bch_ioctl_err_msg,
}

c_const! {
    pub const BCH_SUBVOL_SNAPSHOT_CREATE: u32 = 1 << 0;
}

c_const! {
    pub const BCH_SUBVOL_SNAPSHOT_RO: u32 = 1 << 1;
}

/*
 * BCH_IOCTL_FSCK_OFFLINE: run fsck from the 'bcachefs fsck' userspace command,
 * but with the kernel's implementation of fsck:
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_fsck_offline {
    pub flags: u64,
    pub opts: u64, /* string */
    pub nr_devs: u64,
    /*
     * Do not re-add __counted_by() here: there's a compiler bug that causes
     * a bounds check to happen, even when it's a userspace pointer
     * (properly marked as __user)
     */
    pub devs: [u64; 0],
}

/*
 * BCH_IOCTL_FSCK_ONLINE: run fsck from the 'bcachefs fsck' userspace command,
 * but with the kernel's implementation of fsck:
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_fsck_online {
    pub flags: u64,
    pub opts: u64, /* string */
}

/*
 * BCH_IOCTL_QUERY_ACCOUNTING: query filesystem disk accounting
 *
 * Returns disk space usage broken out by data type, number of replicas, and
 * by component device
 *
 * @replica_entries_bytes - size, in bytes, allocated for replica usage entries
 *
 * On success, @replica_entries_bytes will be changed to indicate the number of
 * bytes actually used.
 *
 * Returns -ERANGE if @replica_entries_bytes was too small
 */
#[repr(C)]
#[derive(Default, CStruct, TypeInfo)]
pub struct bch_ioctl_query_accounting {
    pub capacity: u64,
    pub used: u64,
    pub online_reserved: u64,

    pub accounting_u64s: u32, /* input parameter */
    pub accounting_types_mask: u32, /* input parameter */

    pub accounting: [c::bkey_i_accounting; 0],
}

c_const! {
    /*
     * BCH_IOCTL_QUERY_ACCOUNTING_v2: as v1, plus free space by replica count
     *
     * Free space is a vector, not a scalar: raw sectors free says nothing about
     * whether n copies can go on n distinct devices, so a filesystem can report
     * room and then refuse the write. @free[n - 1] is what we would grant at n
     * replicas - the cumulative figure, so free[0] is the whole of the free space
     * and the numbers are non-increasing.
     *
     * @free_now is the same vector counting only space the allocator can hand out
     * without waiting: @free includes fragmentation copygc hasn't compacted yet, so
     * a write against the difference blocks on copygc rather than failing. The gap
     * is the allocator's backlog, and it's what distinguishes a filesystem that is
     * slow right now from one that is full.
     *
     * Both are sized 8 rather than BCH_REPLICAS_MAX deliberately: sizeof(this
     * struct) is encoded in the ioctl number, so sizing it by a constant that could
     * grow would silently change the command and -ENOTTY every existing binary.
     * Entries from BCH_REPLICAS_MAX up are zero.
     */
    #[c_int]
    pub const BCH_IOCTL_QUERY_ACCOUNTING_FREE_NR: u32 = 8;
}

#[repr(C)]
#[derive(Default, CStruct, TypeInfo)]
pub struct bch_ioctl_query_accounting_v2 {
    pub capacity: u64,
    pub used: u64,
    pub online_reserved: u64,
    pub free: [u64; c::BCH_IOCTL_QUERY_ACCOUNTING_FREE_NR as usize],
    pub free_now: [u64; c::BCH_IOCTL_QUERY_ACCOUNTING_FREE_NR as usize],

    pub accounting_u64s: u32, /* input parameter */
    pub accounting_types_mask: u32, /* input parameter */

    pub accounting: [c::bkey_i_accounting; 0],
}

c_const! {
    #[c_int]
    pub const BCH_IOCTL_QUERY_COUNTERS_MOUNT: u32 = 1 << 0;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_query_counters {
    pub nr: u16,
    pub flags: u16,
    pub pad: u32,
    pub d: [u64; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_subvol_dirent {
    pub reclen: u32,
    pub subvolid: u32,
    pub flags: u32,
    pub snapshot_parent: u32,
    pub otime_sec: u64,
    pub otime_nsec: u32,
    pub pad: u32,
    pub path: [crate::util::ffi::c_char; 0],
}

/*
 * BCH_IOCTL_SUBVOLUME_LIST: list child subvolumes of a given parent,
 * readdir style.
 *
 * Parent subvolume is determined from the directory fd used for the ioctl.
 *
 * @pos		- in/out: cursor (child subvolid); 0 to start
 * @buf_size	- size of buffer in bytes
 * @buf		- pointer to userspace buffer for entries
 * @used	- out: bytes written to buffer
 *
 * Each entry in the buffer is a struct bch_ioctl_subvol_dirent with a
 * variable-length NUL-terminated path (relative to the parent subvolume
 * root), padded to 8-byte alignment.
 *
 * Returns 0 on success (used == 0 means no more entries).
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_subvol_readdir {
    pub pos: u32,
    pub buf_size: u32,
    pub buf: u64,
    pub used: u32,
    pub pad: u32,
}

/*
 * BCH_IOCTL_SUBVOLUME_TO_PATH: resolve a subvolume ID to its filesystem path,
 * relative to the filesystem root.
 *
 * The directory fd only selects the filesystem: the path returned does not
 * depend on which directory the fd names, and is not relative to it.
 *
 * @subvolid	- subvolume ID to resolve
 * @buf_size	- size of userspace buffer in bytes
 * @buf		- pointer to userspace buffer for NUL-terminated path
 *
 * Returns 0 on success, -ENOENT if the subvolume doesn't exist or isn't
 * reachable from the fd, -ERANGE if the buffer is too small.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_subvol_to_path {
    pub subvolid: u32,
    pub buf_size: u32,
    pub buf: u64,
}

/*
 * BCH_IOCTL_SNAPSHOT_TREE: return the full snapshot tree (interior + leaf
 * nodes) with per-node disk accounting.
 *
 * @tree_id	- snapshot tree to query; 0 = infer from fd's subvolume
 * @master_subvol - out: master subvolume of this tree
 * @root_snapshot - out: root snapshot ID
 * @nr		- in: capacity of nodes[]; out: entries returned
 * @total	- out: total nodes in tree
 *
 * Returns -ERANGE if nr < total (nr and total are still written back)
 */
/*
 * FROZEN. The ioctl number encodes sizeof(the header), not of the array
 * element, so growing this struct is invisible to the ioctl machinery and
 * silently overruns the buffer of any userspace built against the older
 * layout. It happened once (nr_keys/key_bytes, reverted); don't do it again -
 * add fields to bch_ioctl_snapshot_node_v2, which is size-negotiated.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_snapshot_node {
    pub id: u32, /* snapshot ID */
    pub parent: u32, /* parent snapshot ID, 0 for root */
    pub children: [u32; 2],
    pub subvol: u32, /* subvolume ID, 0 for interior */
    pub flags: u32,
    pub pad: [u32; 2],
    /* BCH_DISK_ACCOUNTING_snapshot, summed over the snapshot btrees: */
    pub sectors: u64, /* external (on-disk data) sectors */
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_snapshot_tree_query {
    pub tree_id: u32, /* in: 0 = infer from fd's subvol */
    pub master_subvol: u32, /* out */
    pub root_snapshot: u32, /* out */
    pub nr: u32, /* in: capacity; out: returned */
    pub total: u32, /* out: total nodes */
    pub pad: u32,
    pub nodes: [c::bch_ioctl_snapshot_node; 0],
}

/*
 * v2: same query, but the header states the array element size, so either
 * side can grow bch_ioctl_snapshot_node_v2 without breaking the other.
 * The kernel writes min(node_size, its own sizeof) bytes per entry and
 * strides by node_size, so a shorter node from either direction truncates
 * instead of overrunning. New fields go on the end, and both sides read
 * node_size to know what's present.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_snapshot_node_v2 {
    pub id: u32, /* snapshot ID */
    pub parent: u32, /* parent snapshot ID, 0 for root */
    pub children: [u32; 2],
    pub subvol: u32, /* subvolume ID, 0 for interior */
    pub flags: u32,
    pub pad: [u32; 2],
    /* BCH_DISK_ACCOUNTING_snapshot, summed over the snapshot btrees: */
    pub sectors: u64, /* external (on-disk data) sectors */
    pub nr_keys: u64,
    pub key_bytes: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_snapshot_tree_query_v2 {
    pub tree_id: u32, /* in: 0 = infer from fd's subvol */
    pub master_subvol: u32, /* out */
    pub root_snapshot: u32, /* out */
    pub nr: u32, /* in: capacity; out: returned */
    pub total: u32, /* out: total nodes */
    pub node_size: u32, /* in: caller's sizeof; out: ours */
    pub nodes: [c::bch_ioctl_snapshot_node_v2; 0],
}

c_xmacro! {
    /*
     * What a progress indicator is counting: this is uapi because
     * BCH_IOCTL_RECOVERY_STATUS reports it, and a string wouldn't survive the trip.
     */
    BCH_PROGRESS_UNITS(x) {
        (nodes, 0),
        (keys, 1),
    }
}

macro_rules! __bch_progress_units_0 {
    ([$($acc:tt)*] $(($n:tt, $v:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_progress_units: u32 {
                $($acc)*
                $([<BCH_PROGRESS_UNITS_ $n>] = (($v) as u32),)*
            }
        }
    } };
}
BCH_PROGRESS_UNITS!(__bch_progress_units_0 []);

/*
 * A set of enum bch_recovery_pass ids - the in-memory pass ids, not the stable
 * ids the superblock stores. Bit n of v[0] is pass n, bit n of v[1] is pass
 * 64 + n.
 *
 * 128 bits because 64 is not far off: there are 50 passes today. The kernel's
 * own masks are still u64s, so v[1] reads as zero until those widen - which is
 * the point of having the room here now, so that widening isn't an ABI break.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_recovery_pass_mask {
    pub v: [u64; 2],
}

/*
 * BCH_IOCTL_RECOVERY_STATUS: what recovery is doing, so a caller can draw a
 * progress display instead of scraping log lines.
 *
 * Only implemented on the status fd (the "status_fd" fsconfig parameter), which
 * is the only handle anyone has on a filesystem that hasn't finished mounting.
 * There's no wakeup for this: poll() on the status fd means "text to read", not
 * "progress moved", so callers poll on a timer.
 *
 * All the @passes_* masks but @passes_scheduled_sb are read under the lock
 * recovery updates them with, so those are mutually consistent with each other
 * and with @pass. @passes_remaining excludes @pass, so the passes this run will
 * have touched is
 *
 *	passes_complete | passes_remaining | {pass}
 *
 * and that denominator grows if a pass reschedules an earlier one: a bar drawn
 * from it can go backwards, which is the truth.
 *
 * @seen and @total ride along outside that lock, so a caller can see one pass's
 * count next to the next pass's id. It's a progress bar. @total is zero when
 * the running pass has no estimate of its own size - draw an indeterminate
 * spinner rather than an empty bar.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_recovery_status {
    pub passes_scheduled_sb: c::bch_recovery_pass_mask,
    pub passes_scheduled_ephemeral: c::bch_recovery_pass_mask,
    pub passes_complete: c::bch_recovery_pass_mask,
    pub passes_remaining: c::bch_recovery_pass_mask,

    pub pass: u32, /* enum bch_recovery_pass, 0 = idle */
    pub units: u32, /* enum bch_progress_units */
    pub seen: u64,
    pub total: u64,
}

c_const! {
    /*
     * BCHFS_IOC_PREAD_RAW: O_DIRECT read with extended error reporting.
     *
     * Like pread(), but with flags to control error handling and detailed
     * error reporting via the errors bitmask and embedded err_msg.
     *
     * With no flags set, behaves like a normal O_DIRECT read but with
     * better error information.  BCH_PREAD_RAW_no_poison_check bypasses
     * extent poisoning so corrupted data can be recovered.
     */
    pub const BCH_PREAD_RAW_no_poison_check: u32 = 1 << 0;
}

c_const! {
    pub const BCH_PREAD_RAW_ERR_checksum: u32 = 1 << 0;
}

c_const! {
    pub const BCH_PREAD_RAW_ERR_io: u32 = 1 << 1;
}

c_const! {
    pub const BCH_PREAD_RAW_ERR_decompression: u32 = 1 << 2;
}

c_const! {
    pub const BCH_PREAD_RAW_ERR_ec_reconstruct: u32 = 1 << 3;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_pread_raw {
    pub offset: u64,
    pub len: u64,
    pub buf: u64, /* userspace data buffer */
    pub flags: u32, /* BCH_PREAD_RAW_* input flags */
    pub errors: u32, /* output: BCH_PREAD_RAW_ERR_* */
    pub err: c::bch_ioctl_err_msg,
}

/*
 * BCHFS_IOC_UNPOISON: clear the poison flag on extents in a file range.
 *
 * After a checksum error, extents are marked poisoned so subsequent reads
 * return errors without re-reading from disk.  This ioctl clears the
 * poison flag, allowing the data to be read again normally.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_unpoison {
    pub offset: u64,
    pub len: u64,
    pub flags: u32, /* reserved, must be 0 */
    pub pad: u32,
}

c_const! {
    /*
     * BCH_IOCTL_QUERY_BTREE_KEYS: read keys from a btree, range query
     *
     * Stateless: the cursor lives in userspace and each call is self-contained,
     * so concurrent callers don't interact and an aborted caller leaves nothing
     * behind. Modeled on BTRFS_IOC_TREE_SEARCH / FS_IOC_GETFSMAP.
     *
     * The buffer is filled with bkeys (struct bkey_i: unpacked key header
     * followed by the value), densely packed; step to the next with bkey_bytes()
     * (k->u64s * 8). This is the unpacked in-memory format - the same format the
     * update ioctls and libbcachefs speak, not the packed on-disk one.
     *
     * Flags:
     *
     * slots: iterate positions instead of keys - a KEY_TYPE_deleted key is
     * synthesized for every position without one. On extents btrees a single
     * synthesized key covers each hole. This is how you see holes; mind that a
     * wide range in slots mode returns a key per position.
     *
     * prev: iterate backwards, from @start down to @end.
     *
     * all_snapshots: return keys from all snapshots, rather than filtering to
     * the snapshot in @start. Snapshot-filtered iteration (i.e. without this
     * flag, on a snapshots btree) has preconditions, -EINVAL otherwise:
     * @start.snapshot must be nonzero (there's no snapshot to filter against),
     * @end must not be POS_MAX (whiteout filtering peeks ahead of the end pos),
     * and with prev the range must be within a single inode. Interior node
     * levels (@level > 0) always iterate all snapshots.
     *
     * @btree	- btree id
     * @level	- btree level to read keys from (0 = leaves)
     * @flags	- BCH_IOCTL_QUERY_BTREE_KEYS_*
     * @done	- out: nonzero once iteration has reached the end of the range
     * @start	- in/out: cursor; on return, where the next call should
     *		  resume (only meaningful while @done is unset)
     * @end		- inclusive bound: upper, or lower with prev
     * @buf		- pointer to userspace buffer for keys
     * @buf_size	- size of buffer in bytes
     * @used	- out: bytes written to buffer
     *
     * To iterate: repeat the call, keeping @start from the previous call, until
     * @done is set. The kernel bounds how much it returns per call, so @used may
     * be well short of @buf_size while more keys remain.
     *
     * Returns -ERANGE if @buf_size is too small to hold even one key.
     */
    pub const BCH_IOCTL_QUERY_BTREE_KEYS_slots: u32 = 1 << 0;
}

c_const! {
    pub const BCH_IOCTL_QUERY_BTREE_KEYS_prev: u32 = 1 << 1;
}

c_const! {
    pub const BCH_IOCTL_QUERY_BTREE_KEYS_all_snapshots: u32 = 1 << 2;
}

c_const! {
    pub const BCH_IOCTL_QUERY_BTREE_KEYS_nofilter_whiteouts: u32 = 1 << 3;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_ioctl_query_btree_keys {
    pub btree: u32,
    pub level: u32,
    pub flags: u32,
    pub done: u32,
    pub start: c::bpos,
    pub end: c::bpos,
    pub buf: u64,
    pub buf_size: u32,
    pub used: u32,
}
