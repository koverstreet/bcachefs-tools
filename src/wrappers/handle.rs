use std::ffi::CStr;
use std::io;
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

use bch_bindgen::c::{
    bch_data_type,
    bch_ioctl_dev_usage, bch_ioctl_dev_usage_v2,
    bch_ioctl_dev_usage_bch_ioctl_dev_usage_type,
    bch_ioctl_disk, bch_ioctl_disk_v2,
    bch_ioctl_disk_set_state, bch_ioctl_disk_set_state_v2,
    bch_ioctl_disk_resize, bch_ioctl_disk_resize_v2,
    bch_ioctl_disk_resize_journal, bch_ioctl_disk_resize_journal_v2,
    bch_ioctl_subvolume, bch_ioctl_subvolume_v2,
    BCH_BY_INDEX, BCH_SUBVOL_SNAPSHOT_CREATE,
};
use crate::wrappers::ioctl::bch_ioc_wr;
use crate::wrappers::sysfs;
use bch_bindgen::errcode::BchError;
use bch_bindgen::path_to_cstr;
use errno::Errno;
use rustix::ioctl::{self, CompileTimeOpcode, Setter, WriteOpcode};

/// Try a v2 ioctl (with error message buffer), falling back to v1 on ENOTTY.
macro_rules! v2_v1_ioctl {
    ($fd:expr, $V2:ty, $V1:ty, $v2_arg:expr, $v1_arg:expr) => {{
        let mut err_buf = [0u8; 8192];
        let mut arg = $v2_arg;
        arg.err.msg_ptr = err_buf.as_mut_ptr() as u64;
        arg.err.msg_len = err_buf.len() as u32;

        match unsafe { ioctl::ioctl($fd, Setter::<$V2, _>::new(arg)) } {
            Ok(()) => Ok(()),
            Err(e) if e == rustix::io::Errno::NOTTY => {
                unsafe { ioctl::ioctl($fd, Setter::<$V1, _>::new($v1_arg)) }
                    .map_err(|e| Errno(e.raw_os_error()))
            }
            Err(e) => {
                print_errmsg(&err_buf);
                Err(Errno(e.raw_os_error()))
            }
        }
    }};
}

// Subvolume ioctl opcodes
type SubvolCreateOpcode    = WriteOpcode<0xbc, 16, bch_ioctl_subvolume>;
type SubvolCreateV2Opcode  = WriteOpcode<0xbc, 29, bch_ioctl_subvolume_v2>;
type SubvolDestroyOpcode   = WriteOpcode<0xbc, 17, bch_ioctl_subvolume>;
type SubvolDestroyV2Opcode = WriteOpcode<0xbc, 30, bch_ioctl_subvolume_v2>;

// Disk ioctl opcodes (_IOW(0xbc, N, struct))
type DiskAddOpcode         = WriteOpcode<0xbc, 4,  bch_ioctl_disk>;
type DiskAddV2Opcode       = WriteOpcode<0xbc, 23, bch_ioctl_disk_v2>;
type DiskRemoveOpcode      = WriteOpcode<0xbc, 5,  bch_ioctl_disk>;
type DiskRemoveV2Opcode    = WriteOpcode<0xbc, 24, bch_ioctl_disk_v2>;
type DiskOnlineOpcode      = WriteOpcode<0xbc, 6,  bch_ioctl_disk>;
type DiskOnlineV2Opcode    = WriteOpcode<0xbc, 25, bch_ioctl_disk_v2>;
type DiskOfflineOpcode     = WriteOpcode<0xbc, 7,  bch_ioctl_disk>;
type DiskOfflineV2Opcode   = WriteOpcode<0xbc, 26, bch_ioctl_disk_v2>;
type DiskSetStateOpcode    = WriteOpcode<0xbc, 8,  bch_ioctl_disk_set_state>;
type DiskSetStateV2Opcode  = WriteOpcode<0xbc, 22, bch_ioctl_disk_set_state_v2>;
type DiskResizeOpcode      = WriteOpcode<0xbc, 14, bch_ioctl_disk_resize>;
type DiskResizeV2Opcode    = WriteOpcode<0xbc, 27, bch_ioctl_disk_resize_v2>;
type DiskResizeJournalOpcode   = WriteOpcode<0xbc, 15, bch_ioctl_disk_resize_journal>;
type DiskResizeJournalV2Opcode = WriteOpcode<0xbc, 28, bch_ioctl_disk_resize_journal_v2>;

const SYSFS_BASE: &str = "/sys/fs/bcachefs/";

/// BCH_IOCTL_QUERY_UUID: _IOR(0xbc, 1, struct bch_ioctl_query_uuid)
/// Returns the user-visible filesystem UUID.
#[repr(C)]
#[derive(Default)]
struct BchIoctlQueryUuid {
    uuid: [u8; 16],
}

/// Compute _IOR(type, nr, size)
const fn ioc_r(type_: u32, nr: u32, size: u32) -> libc::c_ulong {
    ((2u32 << 30) | (size << 16) | (type_ << 8) | nr) as libc::c_ulong
}

const BCH_IOCTL_QUERY_UUID: libc::c_ulong =
    ioc_r(0xbc, 1, mem::size_of::<BchIoctlQueryUuid>() as u32);

/// FS_IOC_GETFSSYSFSPATH: _IOR(0x15, 1, struct fs_sysfs_path)
#[repr(C)]
struct FsSysfsPath {
    len: u8,
    name: [u8; 128],
}

const FS_IOC_GETFSSYSFSPATH: libc::c_ulong =
    ioc_r(0x15, 1, mem::size_of::<FsSysfsPath>() as u32);

/// A handle to a bcachefs filesystem, with RAII close.
pub(crate) struct BcachefsHandle {
    ioctl_fd: OwnedFd,
    sysfs_fd: OwnedFd,
    uuid:     [u8; 16],
    dev_idx:  i32,
}

impl BcachefsHandle {
    pub(crate) fn sysfs_fd(&self) -> BorrowedFd<'_> {
        self.sysfs_fd.as_fd()
    }

    pub(crate) fn ioctl_fd_raw(&self) -> i32 {
        self.ioctl_fd.as_raw_fd()
    }

    /// Device index when opened via a block device path; -1 when opened via mount point.
    pub(crate) fn dev_idx(&self) -> i32 {
        self.dev_idx
    }

    /// Filesystem UUID.
    pub(crate) fn uuid(&self) -> [u8; 16] {
        self.uuid
    }

    /// Opens a bcachefs filesystem and returns its handle.
    ///
    /// `path` can be:
    /// - A UUID string (e.g. "abcd-...")
    /// - A path to a mounted filesystem
    /// - A block device path
    /// - A file path (reads superblock)
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> Result<Self, BchError> {
        let path = path.as_ref();
        let path_str = path.to_string_lossy();

        // Try as UUID string first
        if let Ok(uuid) = parse_uuid(&path_str) {
            return Self::open_by_name(&path_str, Some(uuid))
                .map_err(|e| BchError::from_raw(-e.0));
        }

        // It's a path — open it
        let path_fd = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        ).map_err(|e| BchError::from_raw(-e.raw_os_error()))?;

        // Try BCH_IOCTL_QUERY_UUID — if it succeeds, it's a mounted fs path
        let mut query_uuid = BchIoctlQueryUuid::default();
        let ret = unsafe {
            libc::ioctl(path_fd.as_raw_fd(), BCH_IOCTL_QUERY_UUID, &mut query_uuid)
        };
        if ret == 0 {
            return Self::open_mounted_path(path_fd, query_uuid.uuid);
        }

        // stat the path to distinguish block device vs file
        let stat = rustix::fs::fstat(&path_fd)
            .map_err(|e| BchError::from_raw(-e.raw_os_error()))?;

        // Drop path_fd — we'll re-open via sysfs/ctl
        drop(path_fd);

        let mode = stat.st_mode & libc::S_IFMT as u32;

        if mode == libc::S_IFBLK as u32 {
            // Block device: try sysfs symlink
            let major = rustix::fs::major(stat.st_rdev);
            let minor = rustix::fs::minor(stat.st_rdev);
            let sysfs_link = format!("/sys/dev/block/{}:{}/bcachefs", major, minor);

            if let Ok(target) = std::fs::read_link(&sysfs_link) {
                let target = target.to_string_lossy();
                // target looks like "../../fs/bcachefs/<uuid>/dev-N"
                // We need to extract uuid and dev_idx
                if let Some((uuid_str, dev_idx)) = parse_sysfs_link(&target) {
                    let uuid = parse_uuid(uuid_str).ok();
                    let mut handle = Self::open_by_name(uuid_str, uuid)
                        .map_err(|e| BchError::from_raw(-e.0))?;
                    handle.dev_idx = dev_idx;
                    return Ok(handle);
                }
            }
        }

        // Fallback: read superblock to get UUID
        Self::open_via_superblock(path)
    }

    /// Open a mounted filesystem path. The fd becomes the ioctl fd.
    fn open_mounted_path(ioctl_fd: OwnedFd, uuid: [u8; 16]) -> Result<Self, BchError> {
        // Try FS_IOC_GETFSSYSFSPATH to get sysfs path
        let mut fs_path = FsSysfsPath { len: 0, name: [0; 128] };
        let ret = unsafe {
            libc::ioctl(ioctl_fd.as_raw_fd(), FS_IOC_GETFSSYSFSPATH, &mut fs_path)
        };

        let sysfs_fd = if ret == 0 {
            let name_len = fs_path.len as usize;
            let name = std::str::from_utf8(&fs_path.name[..name_len])
                .map_err(|_| BchError::from_raw(-libc::EINVAL))?;
            let sysfs = format!("/sys/fs/{}", name);
            rustix::fs::open(
                sysfs.as_str(),
                rustix::fs::OFlags::RDONLY,
                rustix::fs::Mode::empty(),
            ).map_err(|e| BchError::from_raw(-e.raw_os_error()))?
        } else {
            // Fallback: use UUID
            let uuid_str = format_uuid(&uuid);
            let sysfs = format!("{}{}", SYSFS_BASE, uuid_str);
            rustix::fs::open(
                sysfs.as_str(),
                rustix::fs::OFlags::RDONLY,
                rustix::fs::Mode::empty(),
            ).map_err(|e| BchError::from_raw(-e.raw_os_error()))?
        };

        Ok(BcachefsHandle {
            ioctl_fd,
            sysfs_fd,
            uuid,
            dev_idx: -1,
        })
    }

    /// Open by sysfs name (UUID string). Reads minor number, opens /dev/bcachefsN-ctl.
    fn open_by_name(name: &str, uuid: Option<[u8; 16]>) -> Result<Self, Errno> {
        let sysfs_path = format!("{}{}", SYSFS_BASE, name);
        let sysfs_fd = rustix::fs::open(
            sysfs_path.as_str(),
            rustix::fs::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        ).map_err(|e| Errno(e.raw_os_error()))?;

        let minor = sysfs::read_sysfs_fd_str(sysfs_fd.as_fd(), "minor")
            .map_err(|e| Errno(e.raw_os_error().unwrap_or(libc::EIO)))?;

        let ctl_path = format!("/dev/bcachefs{}-ctl", minor);
        let ioctl_fd = rustix::fs::open(
            ctl_path.as_str(),
            rustix::fs::OFlags::RDWR,
            rustix::fs::Mode::empty(),
        ).map_err(|e| Errno(e.raw_os_error()))?;

        Ok(BcachefsHandle {
            ioctl_fd,
            sysfs_fd,
            uuid: uuid.unwrap_or([0; 16]),
            dev_idx: -1,
        })
    }

    /// Open by reading superblock from a device/file path.
    fn open_via_superblock(path: &Path) -> Result<Self, BchError> {
        use bch_bindgen::bcachefs;

        let mut opts = bcachefs::bch_opts::default();
        bch_bindgen::opt_set!(opts, noexcl, 1);
        bch_bindgen::opt_set!(opts, nochanges, 1);

        let sb = bch_bindgen::sb::io::read_super_opts(path, opts)
            .map_err(|e| match e.downcast::<BchError>() {
                Ok(bch_err) => bch_err,
                Err(_) => BchError::from_raw(-libc::EIO),
            })?;

        let dev_idx = unsafe { (*sb.sb).dev_idx as i32 };
        let uuid = unsafe { (*sb.sb).user_uuid.b };
        let uuid_str = format_uuid(&uuid);

        unsafe { bch_bindgen::sb::io::bch2_free_super(&sb as *const _ as *mut _) };

        let mut handle = Self::open_by_name(&uuid_str, Some(uuid))
            .map_err(|e| BchError::from_raw(-e.0))?;
        handle.dev_idx = dev_idx;
        Ok(handle)
    }

    fn ioctl_fd(&self) -> BorrowedFd<'_> {
        self.ioctl_fd.as_fd()
    }

    fn subvol_ioctl<V2: CompileTimeOpcode, V1: CompileTimeOpcode>(
        &self,
        flags: u32,
        dirfd: u32,
        mode: u16,
        dst_ptr: u64,
        src_ptr: u64,
    ) -> Result<(), Errno> {
        v2_v1_ioctl!(
            self.ioctl_fd(), V2, V1,
            bch_ioctl_subvolume_v2 { flags, dirfd, mode, dst_ptr, src_ptr, ..Default::default() },
            bch_ioctl_subvolume    { flags, dirfd, mode, dst_ptr, src_ptr, ..Default::default() }
        )
    }

    /// Create a subvolume for this bcachefs filesystem
    /// at the given path
    pub fn create_subvolume<P: AsRef<Path>>(&self, dst: P) -> Result<(), Errno> {
        let dst = path_to_cstr(dst);
        self.subvol_ioctl::<SubvolCreateV2Opcode, SubvolCreateOpcode>(
            0,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            0,
        )
    }

    /// Delete the subvolume at the given path
    /// for this bcachefs filesystem
    pub fn delete_subvolume<P: AsRef<Path>>(&self, dst: P) -> Result<(), Errno> {
        let dst = path_to_cstr(dst);
        self.subvol_ioctl::<SubvolDestroyV2Opcode, SubvolDestroyOpcode>(
            0,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            0,
        )
    }

    /// Snapshot a subvolume for this bcachefs filesystem
    /// at the given path
    pub fn snapshot_subvolume<P: AsRef<Path>>(
        &self,
        extra_flags: u32,
        src: Option<P>,
        dst: P,
    ) -> Result<(), Errno> {
        let src = src.map(|src| path_to_cstr(src));
        let dst = path_to_cstr(dst);
        self.subvol_ioctl::<SubvolCreateV2Opcode, SubvolCreateOpcode>(
            BCH_SUBVOL_SNAPSHOT_CREATE | extra_flags,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            src.as_ref().map_or(0, |x| x.as_ptr() as u64),
        )
    }

    fn disk_ioctl<V2: CompileTimeOpcode, V1: CompileTimeOpcode>(
        &self, flags: u32, dev: u64,
    ) -> Result<(), Errno> {
        v2_v1_ioctl!(
            self.ioctl_fd(), V2, V1,
            bch_ioctl_disk_v2 { flags, dev, ..Default::default() },
            bch_ioctl_disk    { flags, dev, ..Default::default() }
        )
    }

    /// Add a new device to this filesystem.
    pub(crate) fn disk_add(&self, dev_path: &CStr) -> Result<(), Errno> {
        self.disk_ioctl::<DiskAddV2Opcode, DiskAddOpcode>(
            0, dev_path.as_ptr() as u64,
        )
    }

    /// Remove a device (by index) from this filesystem.
    pub(crate) fn disk_remove(&self, dev_idx: u32, flags: u32) -> Result<(), Errno> {
        self.disk_ioctl::<DiskRemoveV2Opcode, DiskRemoveOpcode>(
            flags | BCH_BY_INDEX, dev_idx as u64,
        )
    }

    /// Re-add an offline device to this filesystem.
    pub(crate) fn disk_online(&self, dev_path: &CStr) -> Result<(), Errno> {
        self.disk_ioctl::<DiskOnlineV2Opcode, DiskOnlineOpcode>(
            0, dev_path.as_ptr() as u64,
        )
    }

    /// Take a device offline without removing it.
    pub(crate) fn disk_offline(&self, dev_idx: u32, flags: u32) -> Result<(), Errno> {
        self.disk_ioctl::<DiskOfflineV2Opcode, DiskOfflineOpcode>(
            flags | BCH_BY_INDEX, dev_idx as u64,
        )
    }

    /// Change device state (rw, ro, evacuating, spare).
    pub(crate) fn disk_set_state(&self, dev_idx: u32, new_state: u32, flags: u32) -> Result<(), Errno> {
        v2_v1_ioctl!(
            self.ioctl_fd(), DiskSetStateV2Opcode, DiskSetStateOpcode,
            bch_ioctl_disk_set_state_v2 { flags: flags | BCH_BY_INDEX, new_state: new_state as u8, dev: dev_idx as u64, ..Default::default() },
            bch_ioctl_disk_set_state    { flags: flags | BCH_BY_INDEX, new_state: new_state as u8, dev: dev_idx as u64, ..Default::default() }
        )
    }

    /// Resize filesystem on a device.
    pub(crate) fn disk_resize(&self, dev_idx: u32, nbuckets: u64) -> Result<(), Errno> {
        v2_v1_ioctl!(
            self.ioctl_fd(), DiskResizeV2Opcode, DiskResizeOpcode,
            bch_ioctl_disk_resize_v2 { flags: BCH_BY_INDEX, dev: dev_idx as u64, nbuckets, ..Default::default() },
            bch_ioctl_disk_resize    { flags: BCH_BY_INDEX, dev: dev_idx as u64, nbuckets, ..Default::default() }
        )
    }

    /// Resize journal on a device.
    pub(crate) fn disk_resize_journal(&self, dev_idx: u32, nbuckets: u64) -> Result<(), Errno> {
        v2_v1_ioctl!(
            self.ioctl_fd(), DiskResizeJournalV2Opcode, DiskResizeJournalOpcode,
            bch_ioctl_disk_resize_journal_v2 { flags: BCH_BY_INDEX, dev: dev_idx as u64, nbuckets, ..Default::default() },
            bch_ioctl_disk_resize_journal    { flags: BCH_BY_INDEX, dev: dev_idx as u64, nbuckets, ..Default::default() }
        )
    }

    /// Query device usage (v2 with flex array, v1 fallback).
    pub(crate) fn dev_usage(&self, dev_idx: u32) -> Result<DevUsage, Errno> {
        let nr_data_types = bch_data_type::BCH_DATA_NR as usize;
        let entry_size = mem::size_of::<bch_ioctl_dev_usage_bch_ioctl_dev_usage_type>();
        let hdr_size = mem::size_of::<bch_ioctl_dev_usage_v2>();
        let buf_size = hdr_size + nr_data_types * entry_size;
        let mut buf = vec![0u8; buf_size];

        // Fill header
        unsafe {
            let hdr = &mut *(buf.as_mut_ptr() as *mut bch_ioctl_dev_usage_v2);
            hdr.dev = dev_idx as u64;
            hdr.flags = BCH_BY_INDEX;
            hdr.nr_data_types = nr_data_types as u8;
        }

        let request = bch_ioc_wr::<bch_ioctl_dev_usage_v2>(18);
        let ret = unsafe { libc::ioctl(self.ioctl_fd_raw(), request, buf.as_mut_ptr()) };

        if ret == 0 {
            // v2 succeeded — parse result
            let hdr = unsafe { &*(buf.as_ptr() as *const bch_ioctl_dev_usage_v2) };
            let actual_nr = hdr.nr_data_types as usize;
            let data_ptr = unsafe { buf.as_ptr().add(hdr_size) }
                as *const bch_ioctl_dev_usage_bch_ioctl_dev_usage_type;

            let mut data_types = Vec::with_capacity(actual_nr);
            for i in 0..actual_nr {
                let d = unsafe { std::ptr::read_unaligned(data_ptr.add(i)) };
                data_types.push(DevUsageType { buckets: d.buckets, sectors: d.sectors, fragmented: d.fragmented });
            }

            return Ok(DevUsage {
                state: hdr.state,
                bucket_size: hdr.bucket_size,
                nr_buckets: hdr.nr_buckets,
                data_types,
            });
        }

        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno != libc::ENOTTY {
            return Err(Errno(errno));
        }

        // v1 fallback
        let mut u_v1 = bch_ioctl_dev_usage {
            dev: dev_idx as u64,
            flags: BCH_BY_INDEX,
            ..Default::default()
        };
        let request_v1 = bch_ioc_wr::<bch_ioctl_dev_usage>(11);
        let ret = unsafe { libc::ioctl(self.ioctl_fd_raw(), request_v1, &mut u_v1 as *mut _) };
        if ret < 0 {
            return Err(Errno(io::Error::last_os_error().raw_os_error().unwrap_or(0)));
        }

        let mut data_types = Vec::new();
        for d in &u_v1.d {
            data_types.push(DevUsageType { buckets: d.buckets, sectors: d.sectors, fragmented: d.fragmented });
        }

        Ok(DevUsage {
            state: u_v1.state,
            bucket_size: u_v1.bucket_size,
            nr_buckets: u_v1.nr_buckets,
            data_types,
        })
    }
}

/// Device disk space usage.
pub(crate) struct DevUsage {
    pub state: u8,
    pub bucket_size: u32,
    pub nr_buckets: u64,
    pub data_types: Vec<DevUsageType>,
}

impl DevUsage {
    /// Iterate data types with their typed enum key.
    /// Caps at BCH_DATA_NR to avoid UB if the kernel returns more types than we know.
    pub fn iter_typed(&self) -> impl Iterator<Item = (bch_data_type, &DevUsageType)> {
        use super::accounting::data_type_from_u8;
        let max = bch_data_type::BCH_DATA_NR as usize;
        self.data_types.iter().enumerate()
            .take(max)
            .map(|(i, dt)| (data_type_from_u8(i as u8), dt))
    }

    /// Total capacity in sectors.
    pub fn capacity_sectors(&self) -> u64 {
        self.nr_buckets * self.bucket_size as u64
    }

    /// Used sectors (all data types except unstriped).
    pub fn used_sectors(&self) -> u64 {
        self.iter_typed()
            .filter(|(t, _)| *t != bch_data_type::BCH_DATA_unstriped)
            .map(|(_, dt)| dt.sectors)
            .sum()
    }
}

/// Per-data-type usage on a device.
pub(crate) struct DevUsageType {
    pub buckets: u64,
    pub sectors: u64,
    pub fragmented: u64,
}

fn print_errmsg(err_buf: &[u8]) {
    if let Ok(msg) = CStr::from_bytes_until_nul(err_buf) {
        if !msg.is_empty() {
            eprintln!("ioctl error: {}", msg.to_string_lossy());
        }
    }
}

/// Parse a UUID string into 16 bytes.
fn parse_uuid(s: &str) -> Result<[u8; 16], ()> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(());
    }
    let mut uuid = [0u8; 16];
    for i in 0..16 {
        uuid[i] = u8::from_str_radix(&hex[i*2..i*2+2], 16).map_err(|_| ())?;
    }
    Ok(uuid)
}

/// Format a UUID as a lowercase hex string with dashes.
fn format_uuid(uuid: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        uuid[0], uuid[1], uuid[2], uuid[3],
        uuid[4], uuid[5],
        uuid[6], uuid[7],
        uuid[8], uuid[9],
        uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15],
    )
}

/// Parse a sysfs bcachefs symlink target like "../../fs/bcachefs/<uuid>/dev-N".
/// Returns (uuid_str, dev_idx).
fn parse_sysfs_link(target: &str) -> Option<(&str, i32)> {
    // Find the last '/' to get "dev-N"
    let (prefix, dev_part) = target.rsplit_once('/')?;
    let dev_idx: i32 = dev_part.strip_prefix("dev-")?.parse().ok()?;

    // Find the uuid — it's the path component before "dev-N"
    let (_, uuid_str) = prefix.rsplit_once('/')?;

    Some((uuid_str, dev_idx))
}
