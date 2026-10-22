use bcachefs_kernel::c;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::path_to_cstr;
use anyhow::anyhow;

pub fn read_super_opts(
    path: &std::path::Path,
    mut opts: c::bch_opts,
) -> anyhow::Result<c::bch_sb_handle> {
    bcachefs_kernel::sb::io::read_super(&path_to_cstr(path), &mut opts).map_err(|e| anyhow!(e))
}

pub fn read_super(path: &std::path::Path) -> anyhow::Result<c::bch_sb_handle> {
    let opts = c::bch_opts::default();
    read_super_opts(path, opts)
}

pub fn read_super_silent(
    path: &std::path::Path,
    mut opts: c::bch_opts,
) -> Result<c::bch_sb_handle, BchError> {
    bcachefs_kernel::sb::io::read_super_silent(&path_to_cstr(path), &mut opts)
}
