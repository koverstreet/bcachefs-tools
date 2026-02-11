use crate::bcachefs;
use crate::c;
use crate::errcode::{BchError, errptr_to_result};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

pub struct Fs {
    pub raw: *mut c::bch_fs,
}

impl Fs {
    /// Access the superblock handle.
    pub fn sb_handle(&self) -> &bcachefs::bch_sb_handle {
        unsafe { &(*self.raw).disk_sb }
    }

    /// Access the superblock.
    pub fn sb(&self) -> &bcachefs::bch_sb {
        self.sb_handle().sb()
    }

    /// Write superblock to disk.
    pub fn write_super(&self) {
        unsafe { c::bch2_write_super(self.raw) };
    }

    pub fn open(devs: &[PathBuf], mut opts: c::bch_opts) -> Result<Fs, BchError> {
        let devs_cstrs : Vec<_> = devs
            .iter()
            .map(|i| CString::new(i.as_os_str().as_bytes()).unwrap())
            .collect();

        let mut devs_array: Vec<_> = devs_cstrs
            .iter()
            .map(|i| i.as_ptr())
            .collect();

        let ret = unsafe {
            let mut devs: c::darray_const_str = std::mem::zeroed();

            devs.data = devs_array[..].as_mut_ptr();
            devs.nr = devs_array.len();

            c::bch2_fs_open(&mut devs, &mut opts)
        };

        errptr_to_result(ret).map(|fs| Fs { raw: fs })
    }

    /// Shut down the filesystem, returning the error code from bch2_fs_exit.
    /// Consumes self so the caller can't use it afterward; forget prevents
    /// Drop from double-freeing.
    pub fn exit(self) -> i32 {
        let ret = unsafe { c::bch2_fs_exit(self.raw) };
        std::mem::forget(self);
        ret
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        unsafe { c::bch2_fs_exit(self.raw); }
    }
}
