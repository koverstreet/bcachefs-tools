use bcachefs_kernel::c;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::path_to_cstr;
use anyhow::anyhow;

pub use c::bch2_free_super;

pub fn read_super_opts(
    path: &std::path::Path,
    mut opts: c::bch_opts,
) -> anyhow::Result<c::bch_sb_handle> {
    let path = path_to_cstr(path);
    let mut sb = std::mem::MaybeUninit::zeroed();

    let ret =
        unsafe { c::bch2_read_super(path.as_ptr(), &mut opts, sb.as_mut_ptr()) };

    if ret != 0 {
        Err(anyhow!(BchError::from_raw(ret)))
    } else {
        Ok(unsafe { sb.assume_init() })
    }
}

pub fn read_super(path: &std::path::Path) -> anyhow::Result<c::bch_sb_handle> {
    let opts = c::bch_opts::default();
    read_super_opts(path, opts)
}

pub fn read_super_silent(
    path: &std::path::Path,
    mut opts: c::bch_opts,
) -> Result<c::bch_sb_handle, BchError> {
    let path = path_to_cstr(path);
    let mut sb = std::mem::MaybeUninit::zeroed();

    let ret = unsafe {
        c::bch2_read_super_silent(path.as_ptr(), &mut opts, sb.as_mut_ptr())
    };

    if ret != 0 {
        Err(BchError::from_raw(ret))
    } else {
        Ok(unsafe { sb.assume_init() })
    }
}
