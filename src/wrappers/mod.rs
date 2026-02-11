pub mod accounting;
pub mod handle;
pub mod ioctl;
pub mod printbuf;
pub mod sysfs;

/// Convert a bcachefs error code to a human-readable string.
pub fn bch_err_str(err: i32) -> std::borrow::Cow<'static, str> {
    unsafe { std::ffi::CStr::from_ptr(bch_bindgen::c::bch2_err_str(err)).to_string_lossy() }
}
