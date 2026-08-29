use std::ffi::CStr;
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

use bch_bindgen::c::{
    bch_ioctl_dev_usage, bch_ioctl_dev_usage_v2,
    bch_ioctl_dev_usage_bch_ioctl_dev_usage_type,
    bch_ioctl_disk, bch_ioctl_disk_v2,
    bch_ioctl_disk_set_state, bch_ioctl_disk_set_state_v2,
    bch_ioctl_disk_resize, bch_ioctl_disk_resize_v2,
    bch_ioctl_disk_resize_journal, bch_ioctl_disk_resize_journal_v2,
    bch_ioctl_subvolume, bch_ioctl_subvolume_v2,
    bch_ioctl_query_btree_keys, bch_ioctl_query_uuid, bch_ioctl_read_super,
    BCH_BY_INDEX, BCH_SUBVOL_SNAPSHOT_CREATE,
};
use bch_bindgen::accounting::data_type;
use crate::wrappers::ioctl::*;
use crate::wrappers::sysfs;
use bch_bindgen::c::bch_sb;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::path_to_cstr;
use errno::Errno;

fn io_errno(e: std::io::Error) -> Errno {
    Errno(e.raw_os_error().unwrap_or(libc::EIO))
}

/// Try a v2 ioctl (with error message buffer), falling back to v1 on ENOTTY.
macro_rules! v2_v1_ioctl {
    ($fd:expr, $V2:ty, $V1:ty, $v2_arg:expr, $v1_arg:expr) => {{
        let mut err_buf = [0u8; 8192];
        let mut arg = $v2_arg;
        arg.err.msg_ptr = err_buf.as_mut_ptr() as u64;
        arg.err.msg_len = err_buf.len() as u32;

        match ioctl_w::<$V2>($fd, &arg) {
            Ok(_) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::ENOTTY) =>
                ioctl_w::<$V1>($fd, &$v1_arg).map(|_| ()).map_err(io_errno),
            Err(e) => {
                print_errmsg(&err_buf);
                Err(io_errno(e))
            }
        }
    }};
}

const SYSFS_BASE: &str = "/sys/fs/bcachefs/";

/// FS_IOC_GETFSSYSFSPATH: _IOR(0x15, 1, struct fs_sysfs_path) — generic
/// VFS ioctl (linux/fs.h), not in our generated inventory.
#[repr(C)]
struct FsSysfsPath {
    len: u8,
    name: [u8; 128],
}

const FS_IOC_GETFSSYSFSPATH: libc::Ioctl =
    ((2u32 << 30) | ((mem::size_of::<FsSysfsPath>() as u32) << 16) | (0x15 << 8) | 1) as libc::Ioctl;

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

        // Try as UUID string first (normalized: the sysfs dir is canonical
        // lowercase-with-dashes, the user's spelling may not be)
        if let Ok(uuid) = parse_uuid(&path_str) {
            return Self::open_by_name(&format_uuid(&uuid), Some(uuid))
                .map_err(|e| BchError::from_raw(-e.0));
        }

        if let Some(handle) = Self::open_if_mounted(path)? {
            return Ok(handle);
        }

        // Fallback: read superblock to get UUID
        Self::open_via_superblock(path)
    }

    /// Opens the filesystem a path belongs to, if it's currently mounted:
    /// a UUID, a path on a mounted filesystem, or a block device that's a
    /// member of one. Returns Ok(None) — instead of falling back to reading
    /// the superblock — when the path doesn't resolve to a mounted
    /// filesystem.
    ///
    /// Regular files are never resolved: a filesystem image is not itself a
    /// mounted filesystem, and an image stored on a mounted bcachefs would
    /// otherwise resolve to the outer filesystem. Callers treat images as
    /// offline superblocks.
    pub(crate) fn open_if_mounted<P: AsRef<Path>>(path: P) -> Result<Option<Self>, BchError> {
        let path = path.as_ref();

        if let Ok(uuid) = parse_uuid(&path.to_string_lossy()) {
            // No sysfs dir for the UUID means not mounted; any other error
            // (e.g. EACCES on the ctl device) is real and must not be
            // mistaken for "not mounted", or callers fall back to offline
            // superblock access on a live filesystem. Normalize the string
            // through parse+format: the sysfs dir is canonical lowercase
            // with dashes, the user's spelling may not be:
            return match Self::open_by_name(&format_uuid(&uuid), Some(uuid)) {
                Ok(h) => Ok(Some(h)),
                Err(e) if e.0 == libc::ENOENT => Ok(None),
                Err(e) => Err(BchError::from_raw(-e.0)),
            };
        }

        let Ok(path_fd) = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        ) else {
            return Ok(None);
        };

        let stat = rustix::fs::fstat(&path_fd)
            .map_err(|e| BchError::from_raw(-e.raw_os_error()))?;
        let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);

        if file_type == rustix::fs::FileType::RegularFile {
            return Ok(None);
        }

        // Try BCH_IOCTL_QUERY_UUID — if it succeeds, it's a mounted fs path
        let mut query_uuid = bch_ioctl_query_uuid::default();
        if ioctl_rw::<BCH_IOCTL_QUERY_UUID>(&path_fd, &mut query_uuid).is_ok() {
            return Self::open_mounted_path(path_fd, query_uuid.uuid.b).map(Some);
        }

        // Drop path_fd — we'll re-open via sysfs/ctl
        drop(path_fd);

        if file_type == rustix::fs::FileType::BlockDevice {
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
                    return Ok(Some(handle));
                }
            }
        }

        Ok(None)
    }

    /// Multi-device form of open_if_mounted(): open the filesystem if any
    /// of the given paths resolves to a mounted one.
    pub(crate) fn open_if_mounted_any<P: AsRef<Path>>(paths: &[P]) -> Result<Option<Self>, BchError> {
        for p in paths {
            if let Some(h) = Self::open_if_mounted(p)? {
                return Ok(Some(h));
            }
        }
        Ok(None)
    }

    /// The mounted filesystem's member block device paths, from sysfs
    /// (/sys/fs/bcachefs/<uuid>/dev-N/block). The paths a caller resolved
    /// the filesystem BY (mount point, UUID) aren't openable as devices -
    /// these are.
    pub(crate) fn member_devices(&self) -> Result<Vec<std::path::PathBuf>, BchError> {
        let sysfs_path = sysfs::sysfs_path_from_fd(self.sysfs_fd())
            .map_err(|_| BchError::from_raw(-libc::EIO))?;

        let mut devs = Vec::new();
        for d in sysfs::fs_get_devices(&sysfs_path, sysfs::DeviceNameMode::Raw)
                .map_err(|_| BchError::from_raw(-libc::EIO))? {
            if d.online {
                devs.push(std::path::PathBuf::from(format!("/dev/{}", d.dev)));
            }
        }

        if devs.is_empty() {
            return Err(BchError::from_raw(-libc::ENOENT));
        }
        Ok(devs)
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
        use bcachefs_kernel::c;

        let mut opts = c::bch_opts::default();
        bcachefs_kernel::opt_set!(opts, noexcl, 1);
        bcachefs_kernel::opt_set!(opts, nochanges, 1);

        let sb = bch_bindgen::sb::io::read_super_opts(path, opts)
            .map_err(|e| match e.downcast::<BchError>() {
                Ok(bch_err) => bch_err,
                Err(_) => BchError::from_raw(-libc::EIO),
            })?;

        let dev_idx = sb.sb().dev_idx as i32;
        let uuid = sb.sb().user_uuid.b;
        let uuid_str = format_uuid(&uuid);

        unsafe { bch_bindgen::sb::io::bch2_free_super(&sb as *const _ as *mut _) };

        let mut handle = Self::open_by_name(&uuid_str, Some(uuid))
            .map_err(|e| {
                if e.0 == libc::ENOENT {
                    if !Path::new("/sys/fs/bcachefs").exists() {
                        eprintln!("bcachefs kernel module not loaded");
                    } else {
                        eprintln!("filesystem {} not mounted", uuid_str);
                    }
                }
                BchError::from_raw(-e.0)
            })?;
        handle.dev_idx = dev_idx;
        Ok(handle)
    }

    pub(crate) fn ioctl_fd(&self) -> BorrowedFd<'_> {
        self.ioctl_fd.as_fd()
    }

    fn subvol_ioctl<V2, V1>(
        &self,
        flags: u32,
        dirfd: u32,
        mode: u16,
        dst_ptr: u64,
        src_ptr: u64,
    ) -> Result<(), Errno>
    where
        V2: Ioctl<Arg = bch_ioctl_subvolume_v2>,
        V1: Ioctl<Arg = bch_ioctl_subvolume>,
    {
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
        self.subvol_ioctl::<BCH_IOCTL_SUBVOLUME_CREATE_v2, BCH_IOCTL_SUBVOLUME_CREATE>(
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
        self.subvol_ioctl::<BCH_IOCTL_SUBVOLUME_DESTROY_v2, BCH_IOCTL_SUBVOLUME_DESTROY>(
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
        self.subvol_ioctl::<BCH_IOCTL_SUBVOLUME_CREATE_v2, BCH_IOCTL_SUBVOLUME_CREATE>(
            BCH_SUBVOL_SNAPSHOT_CREATE | extra_flags,
            libc::AT_FDCWD as u32,
            0o777,
            dst.as_ptr() as u64,
            src.as_ref().map_or(0, |x| x.as_ptr() as u64),
        )
    }

    fn disk_ioctl<V2, V1>(&self, flags: u32, dev: u64) -> Result<(), Errno>
    where
        V2: Ioctl<Arg = bch_ioctl_disk_v2>,
        V1: Ioctl<Arg = bch_ioctl_disk>,
    {
        v2_v1_ioctl!(
            self.ioctl_fd(), V2, V1,
            bch_ioctl_disk_v2 { flags, dev, ..Default::default() },
            bch_ioctl_disk    { flags, dev, ..Default::default() }
        )
    }

    /// Add a new device to this filesystem.
    pub(crate) fn disk_add(&self, dev_path: &CStr) -> Result<(), Errno> {
        self.disk_ioctl::<BCH_IOCTL_DISK_ADD_v2, BCH_IOCTL_DISK_ADD>(
            0, dev_path.as_ptr() as u64,
        )
    }

    /// Remove a device (by index) from this filesystem.
    pub(crate) fn disk_remove(&self, dev_idx: u32, flags: u32) -> Result<(), Errno> {
        self.disk_ioctl::<BCH_IOCTL_DISK_REMOVE_v2, BCH_IOCTL_DISK_REMOVE>(
            flags | BCH_BY_INDEX, dev_idx as u64,
        )
    }

    /// Re-add an offline device to this filesystem.
    pub(crate) fn disk_online(&self, dev_path: &CStr) -> Result<(), Errno> {
        self.disk_ioctl::<BCH_IOCTL_DISK_ONLINE_v2, BCH_IOCTL_DISK_ONLINE>(
            0, dev_path.as_ptr() as u64,
        )
    }

    /// Take a device offline without removing it.
    pub(crate) fn disk_offline(&self, dev_idx: u32, flags: u32) -> Result<(), Errno> {
        self.disk_ioctl::<BCH_IOCTL_DISK_OFFLINE_v2, BCH_IOCTL_DISK_OFFLINE>(
            flags | BCH_BY_INDEX, dev_idx as u64,
        )
    }

    /// Change device state (rw, ro, evacuating, spare).
    pub(crate) fn disk_set_state(&self, dev_idx: u32, new_state: u32, flags: u32) -> Result<(), Errno> {
        v2_v1_ioctl!(
            self.ioctl_fd(), BCH_IOCTL_DISK_SET_STATE_v2, BCH_IOCTL_DISK_SET_STATE,
            bch_ioctl_disk_set_state_v2 { flags: flags | BCH_BY_INDEX, new_state: new_state as u8, dev: dev_idx as u64, ..Default::default() },
            bch_ioctl_disk_set_state    { flags: flags | BCH_BY_INDEX, new_state: new_state as u8, dev: dev_idx as u64, ..Default::default() }
        )
    }

    /// Resize filesystem on a device.
    pub(crate) fn disk_resize(&self, dev_idx: u32, nbuckets: u64) -> Result<(), Errno> {
        v2_v1_ioctl!(
            self.ioctl_fd(), BCH_IOCTL_DISK_RESIZE_v2, BCH_IOCTL_DISK_RESIZE,
            bch_ioctl_disk_resize_v2 { flags: BCH_BY_INDEX, dev: dev_idx as u64, nbuckets, ..Default::default() },
            bch_ioctl_disk_resize    { flags: BCH_BY_INDEX, dev: dev_idx as u64, nbuckets, ..Default::default() }
        )
    }

    /// Resize journal on a device.
    pub(crate) fn disk_resize_journal(&self, dev_idx: u32, nbuckets: u64) -> Result<(), Errno> {
        v2_v1_ioctl!(
            self.ioctl_fd(), BCH_IOCTL_DISK_RESIZE_JOURNAL_v2, BCH_IOCTL_DISK_RESIZE_JOURNAL,
            bch_ioctl_disk_resize_journal_v2 { flags: BCH_BY_INDEX, dev: dev_idx as u64, nbuckets, ..Default::default() },
            bch_ioctl_disk_resize_journal    { flags: BCH_BY_INDEX, dev: dev_idx as u64, nbuckets, ..Default::default() }
        )
    }

    /// Read the filesystem superblock via BCH_IOCTL_READ_SUPER.
    ///
    /// Returns a heap-allocated buffer containing the raw superblock.
    /// The kernel may return ERANGE if the buffer is too small, so we
    /// start with a reasonable size and retry once if needed.
    pub(crate) fn read_super(&self) -> Result<Vec<u8>, Errno> {
        let mut size: usize = 4096;

        loop {
            let mut buf = vec![0u8; size];

            let arg = bch_ioctl_read_super {
                size: size as u64,
                sb:   buf.as_mut_ptr() as u64,
                ..Default::default()
            };

            match ioctl_w::<BCH_IOCTL_READ_SUPER>(self.ioctl_fd(), &arg) {
                Ok(_) => return Ok(buf),
                Err(e) if e.raw_os_error() == Some(libc::ERANGE) && size < 1 << 20 =>
                    size *= 4,
                Err(e) => return Err(io_errno(e)),
            }
        }
    }

    /// BCH_IOCTL_QUERY_BTREE_KEYS: fetch one buffer's worth of formatted
    /// keys from a btree range. The cursor lives in `arg` — the kernel
    /// advances `arg.start` past the last key returned; loop until
    /// `arg.done` is set. Returns ERANGE if `arg.buf_size` can't hold even
    /// a single key.
    pub(crate) fn query_btree_keys(&self, arg: &mut bch_ioctl_query_btree_keys) -> Result<(), Errno> {
        ioctl_rw::<BCH_IOCTL_QUERY_BTREE_KEYS>(self.ioctl_fd(), arg)
            .map(|_| ()).map_err(io_errno)
    }

    /// Read the on-disk metadata version from the filesystem superblock.
    pub(crate) fn sb_version(&self) -> Result<u16, Errno> {
        let buf = self.read_super()?;
        if buf.len() < mem::size_of::<bch_sb>() {
            return Err(Errno(libc::EIO));
        }
        let sb = unsafe { &*(buf.as_ptr() as *const bch_sb) };
        Ok(sb.version)
    }

    /// Test a bit of the superblock's compat feature set.
    ///
    /// A compat bit means "this optional thing has been computed". A clear bit
    /// therefore means the data is absent or stale, not that it is zero, so
    /// callers should not present it as a count.
    pub(crate) fn sb_has_compat(&self, bit: u32) -> Result<bool, Errno> {
        let buf = self.read_super()?;
        if buf.len() < mem::size_of::<bch_sb>() {
            return Err(Errno(libc::EIO));
        }
        let sb = unsafe { &*(buf.as_ptr() as *const bch_sb) };
        Ok(u64::from_le(sb.compat[0]) & (1u64 << bit) != 0)
    }

    /// Query device usage (v2 with flex array, v1 fallback).
    pub(crate) fn dev_usage(&self, dev_idx: u32) -> Result<DevUsage, Errno> {
        let nr_data_types = data_type::nr.0 as usize;

        let mut buf = IoctlBuf::<bch_ioctl_dev_usage_v2>::new::<bch_ioctl_dev_usage_bch_ioctl_dev_usage_type>(nr_data_types);
        let hdr = buf.hdr_mut();
        hdr.dev = dev_idx as u64;
        hdr.flags = BCH_BY_INDEX;
        hdr.nr_data_types = nr_data_types as u8;

        let ret = unsafe {
            ioctl_ptr::<BCH_IOCTL_DEV_USAGE_V2>(self.ioctl_fd(), buf.as_mut_ptr())
        };

        if ret.is_ok() {
            let hdr = buf.hdr();
            let nr = (hdr.nr_data_types as usize).min(nr_data_types);

            return Ok(DevUsage {
                state: hdr.state,
                bucket_size: hdr.bucket_size,
                nr_buckets: hdr.nr_buckets,
                data_types: unsafe { hdr.d.as_slice(nr) }.iter()
                    .map(|d| DevUsageType { buckets: d.buckets, sectors: d.sectors, fragmented: d.fragmented })
                    .collect(),
            });
        }

        let err = ret.unwrap_err();
        if err.raw_os_error() != Some(libc::ENOTTY) {
            return Err(io_errno(err));
        }

        // v1 fallback
        let mut u_v1 = bch_ioctl_dev_usage {
            dev: dev_idx as u64,
            flags: BCH_BY_INDEX,
            ..Default::default()
        };
        ioctl_rw::<BCH_IOCTL_DEV_USAGE>(self.ioctl_fd(), &mut u_v1).map_err(io_errno)?;

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
    pub fn iter_typed(&self) -> impl Iterator<Item = (data_type, &DevUsageType)> {
        use super::accounting::data_type_from_u8;
        let max = data_type::nr.0 as usize;
        self.data_types.iter().enumerate()
            .take(max)
            .map(|(i, dt)| (data_type_from_u8(i as u8), dt))
    }

    /// Total capacity in sectors.
    pub fn capacity_sectors(&self) -> u64 {
        self.nr_buckets * self.bucket_size as u64
    }

    /// Hidden sectors (superblock + journal) — subtracted from capacity for percentage display.
    pub fn hidden_sectors(&self) -> u64 {
        use super::accounting::data_type_is_hidden;
        self.iter_typed()
            .filter(|(t, _)| data_type_is_hidden(*t))
            .map(|(_, dt)| dt.sectors)
            .sum()
    }

    /// Used sectors (all data types except unstriped).
    pub fn used_sectors(&self) -> u64 {
        self.iter_typed()
            .filter(|(t, _)| *t != data_type::unstriped)
            .map(|(_, dt)| dt.sectors)
            .sum()
    }

    /// Used buckets (excludes free/need_gc_gens/need_discard and hidden types).
    pub fn used_buckets(&self) -> u64 {
        use super::accounting::{data_type_is_empty, data_type_is_hidden};
        self.iter_typed()
            .filter(|(t, _)| !data_type_is_empty(*t) && !data_type_is_hidden(*t))
            .map(|(_, dt)| dt.buckets)
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
