use std::ffi::CStr;
use std::mem;
use std::os::fd::BorrowedFd;
use std::path::Path;

use bch_bindgen::c::{
    bcache_fs_close, bcache_fs_open_fallible,
    bch_data_type,
    bch_ioctl_dev_usage, bch_ioctl_dev_usage_v2,
    bch_ioctl_dev_usage_bch_ioctl_dev_usage_type,
    bch_ioctl_disk, bch_ioctl_disk_v2,
    bch_ioctl_disk_set_state, bch_ioctl_disk_set_state_v2,
    bch_ioctl_disk_resize, bch_ioctl_disk_resize_v2,
    bch_ioctl_disk_resize_journal, bch_ioctl_disk_resize_journal_v2,
    bch_ioctl_subvolume, bch_ioctl_subvolume_v2,
    bchfs_handle,
    BCH_BY_INDEX, BCH_SUBVOL_SNAPSHOT_CREATE,
};
use crate::wrappers::ioctl::bch_ioc_wr;
use bch_bindgen::errcode::{BchError, ret_to_result};
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

/// A handle to a bcachefs filesystem
/// This can be used to send [`libc::ioctl`] to the underlying filesystem.
pub(crate) struct BcachefsHandle {
    inner: bchfs_handle,
}

impl BcachefsHandle {
    pub(crate) fn sysfs_fd(&self) -> i32 {
        self.inner.sysfs_fd
    }

    pub(crate) fn ioctl_fd_raw(&self) -> i32 {
        self.inner.ioctl_fd
    }

    /// Device index when opened via a block device path; -1 when opened via mount point.
    pub(crate) fn dev_idx(&self) -> i32 {
        self.inner.dev_idx
    }

    /// Filesystem UUID.
    pub(crate) fn uuid(&self) -> [u8; 16] {
        self.inner.uuid.b
    }

    /// Opens a bcachefs filesystem and returns its handle
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> Result<Self, BchError> {
        let path = path_to_cstr(path);
        let mut handle = std::mem::MaybeUninit::uninit();
        ret_to_result(unsafe { bcache_fs_open_fallible(path.as_ptr(), handle.as_mut_ptr()) })?;
        let inner = unsafe { handle.assume_init() };
        Ok(Self { inner })
    }

    fn ioctl_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.ioctl_fd_raw()) }
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

        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
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
            return Err(Errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)));
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

impl Drop for BcachefsHandle {
    fn drop(&mut self) {
        unsafe { bcache_fs_close(self.inner) };
    }
}
