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
use bcachefs_kernel::errcode::{self, BchError};
use bcachefs_kernel::fs::{Fs, OpenFs};
use bcachefs_kernel::util::printbuf::Printbuf;

use crate::data::io::{ReadOp, WriteOp};

/// Userspace extension methods on the core [`Fs`].
pub trait FsExt {
    fn open(devs: &[PathBuf], opts: c::bch_opts) -> Result<OpenFs, BchError>;
    fn open_with_key(devs: &[PathBuf], opts: c::bch_opts, user_key: Option<&c::bch_key>)
        -> Result<OpenFs, BchError>;
    fn write(&self, inum: u64, offset: u64, subvol: u32, replicas: u32, data: &[u8], new_i_size: u64) -> WriteOp;
    fn read<'a>(&'a self, inum: c::subvol_inum, offset: u64, inode: &c::bch_inode_unpacked, buf: &'a mut [u8]) -> ReadOp;
    fn dev_add(&self, path: &str) -> Result<(), BchError>;
}

impl FsExt for Fs {
    fn open(devs: &[PathBuf], opts: c::bch_opts) -> Result<OpenFs, BchError> {
        Self::open_with_key(devs, opts, None)
    }

    /// @user_key: the passphrase-derived key, for a passphrase-protected
    /// filesystem. Without it the open looks in the keyring, and failing
    /// that prompts for the passphrase on stdin itself (bch2_request_key()).
    fn open_with_key(devs: &[PathBuf], mut opts: c::bch_opts, user_key: Option<&c::bch_key>)
        -> Result<OpenFs, BchError>
    {
        let devs: Vec<_> = devs
            .iter()
            .map(|i| CString::new(i.as_os_str().as_bytes()).unwrap())
            .collect();

        OpenFs::open(devs.iter().map(CString::as_c_str), &mut opts, user_key)
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


    fn dev_add(&self, path: &str) -> Result<(), BchError> {
        let path_cstr = CString::new(path).unwrap();
        let mut err = Printbuf::new();
        let ret = unsafe { c::bch2_dev_add(self.as_ptr(), path_cstr.as_ptr(), err.as_raw()) };
        if ret != 0 {
            match err.as_str().trim_end() {
                ""  => eprintln!("error adding device {path}"),
                msg => eprintln!("error adding device {path}: {msg}"),
            }
        }
        errcode::ret_to_result(ret).map(|_| ())
    }
}
