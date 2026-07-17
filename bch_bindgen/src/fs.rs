// SPDX-License-Identifier: GPL-2.0
//! Userspace-only `Fs` methods.
//!
//! Filesystem open (device paths), buffered read/write over the userspace
//! data/io ops, accounting readout, and device add — all depend on std/alloc
//! or the userspace data/io wrappers. They can't be inherent methods on the
//! core `Fs` from this crate (orphan rule), so they're an extension trait;
//! callers `use bch_bindgen::fs::FsExt` to get the method syntax back.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::c;
use bcachefs_kernel::errcode::{self, BchError, errptr_to_result};
use bcachefs_kernel::fs::Fs;

use crate::data::io::{ReadOp, WriteOp};

extern "C" {
    fn rust_accounting_mem_read(c: *mut c::bch_fs, p: c::bpos, v: *mut u64, nr: u32);
}

/// Userspace extension methods on the core [`Fs`].
pub trait FsExt {
    fn open(devs: &[PathBuf], opts: c::bch_opts) -> Result<Fs, BchError>;
    fn write(&self, inum: u64, offset: u64, subvol: u32, replicas: u32, data: &[u8], new_i_size: u64) -> WriteOp;
    fn read<'a>(&'a self, inum: c::subvol_inum, offset: u64, inode: &c::bch_inode_unpacked, buf: &'a mut [u8]) -> ReadOp;
    fn accounting_mem_read(&self, pos: c::bpos, nr: u32) -> Vec<u64>;
    fn dev_add(&self, path: &str) -> Result<(), BchError>;
}

impl FsExt for Fs {
    fn open(devs: &[PathBuf], mut opts: c::bch_opts) -> Result<Fs, BchError> {
        let devs_cstrs: Vec<_> = devs
            .iter()
            .map(|i| CString::new(i.as_os_str().as_bytes()).unwrap())
            .collect();

        let mut devs_array: Vec<_> = devs_cstrs.iter().map(|i| i.as_ptr()).collect();

        let ret = unsafe {
            let mut devs: c::darray_const_str = std::mem::zeroed();
            devs.data = devs_array[..].as_mut_ptr();
            devs.nr = devs_array.len();
            c::bch2_fs_open(&mut devs, &mut opts)
        };

        errptr_to_result(ret).map(|fs| Fs { raw: fs })
    }

    fn write(&self, inum: u64, offset: u64, subvol: u32, replicas: u32, data: &[u8], new_i_size: u64) -> WriteOp {
        WriteOp::new(self, inum, offset, subvol, replicas, data, new_i_size)
    }

    fn read<'a>(
        &'a self,
        inum: c::subvol_inum,
        offset: u64,
        inode: &c::bch_inode_unpacked,
        buf: &'a mut [u8],
    ) -> ReadOp {
        ReadOp::new(self, inum, offset, inode, buf)
    }

    fn accounting_mem_read(&self, pos: c::bpos, nr: u32) -> Vec<u64> {
        let mut v = vec![0u64; nr as usize];
        unsafe {
            rust_accounting_mem_read(self.raw, pos, v.as_mut_ptr(), nr);
        }
        v
    }

    fn dev_add(&self, path: &str) -> Result<(), BchError> {
        let path_cstr = CString::new(path).unwrap();
        let mut err = c::printbuf::new();
        let ret = unsafe { c::bch2_dev_add(self.raw, path_cstr.as_ptr(), &mut err) };
        if ret != 0 {
            let msg = unsafe { std::ffi::CStr::from_ptr(err.buf) }.to_string_lossy();
            eprintln!("error adding device: {}", msg);
        }
        errcode::ret_to_result(ret).map(|_| ())
    }
}
