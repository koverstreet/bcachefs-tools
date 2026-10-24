pub mod accounting;
pub mod bdev;
pub mod handle;
pub mod ioctl;
pub mod journal;
pub mod online_iter;
pub mod sb_display;
pub mod super_io;
pub mod sysfs;

/// Convert a bcachefs error code to a human-readable string.
pub fn bch_err_str(err: i32) -> std::borrow::Cow<'static, str> {
    bcachefs_kernel::errcode::BchError::from_raw(err).msg().into()
}
