use crate::c;
use crate::errcode::{bch_errcode, errptr_to_result};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

pub struct Fs {
    pub raw: *mut c::bch_fs,
}

impl Fs {
    pub fn open(devs: &Vec<PathBuf>, opts: c::bch_opts) -> Result<Fs, bch_errcode> {
        let devs: Vec<_> = devs
            .iter()
            .map(|i| CString::new(i.as_os_str().as_bytes()).unwrap().into_raw())
            .collect();

        let ret = unsafe { c::bch2_fs_open(devs[..].as_ptr(), devs.len() as u32, opts) };

        errptr_to_result(ret).map(|fs| Fs { raw: fs })
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        unsafe { c::bch2_fs_stop(self.raw) }
    }
}
