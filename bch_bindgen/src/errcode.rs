use crate::bcachefs;
use crate::c;
use std::ffi::{c_int, CStr};
use std::fmt;

pub use crate::c::bch_errcode;

/// Safe wrapper for bcachefs/errno error codes.
/// Stores the positive error code — either a standard errno (1..2047)
/// or a bcachefs-specific code (BCH_ERR_START=2048..).
///
/// Unlike `bch_errcode` (a repr(u32) enum), this never creates an invalid
/// enum discriminant from a raw errno value.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct BchError(i32);

impl BchError {
    pub fn from_raw(code: i32) -> Self { Self(code) }
    pub fn raw(&self) -> i32 { self.0 }

    /// Get the error message string.
    ///
    /// Returns a static string since bch2_err_str() returns strings
    /// that live for the process lifetime.
    pub fn msg(&self) -> &'static str {
        unsafe { CStr::from_ptr(bcachefs::bch2_err_str(self.0)) }
            .to_str()
            .unwrap_or("unknown error")
    }

    pub fn matches(&self, class: bch_errcode) -> bool {
        if self.0 != 0 {
            unsafe { c::__bch2_err_matches(self.0, class as i32) }
        } else {
            false
        }
    }

    pub fn matches_errno(&self, class: i32) -> bool {
        if self.0 != 0 {
            unsafe { c::__bch2_err_matches(self.0, class) }
        } else {
            false
        }
    }

    /// Return the standard errno that this error maps to.
    /// bcachefs error codes (>= 2048) walk the parent chain to their
    /// root errno; standard errnos pass through unchanged.
    pub fn errno(&self) -> i32 {
        if self.0 == 0 {
            0
        } else {
            // __bch2_err_class takes and returns negative error codes
            -unsafe { c::__bch2_err_class(-self.0) }
        }
    }
}

impl fmt::Display for BchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.msg())
    }
}

impl fmt::Debug for BchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BchError({}, {})", self.0, self)
    }
}

impl std::error::Error for BchError {}

pub fn ret_to_result(ret: c_int) -> Result<c_int, BchError> {
    if ret < 0 && ret > -4096 {
        Err(BchError(-ret))
    } else {
        Ok(ret)
    }
}

pub fn errptr_to_result<T>(p: *mut T) -> Result<*mut T, BchError> {
    let addr = p as usize;
    let max_err: isize = -4096;
    if addr > max_err as usize {
        Err(BchError(-(addr as i32)))
    } else {
        Ok(p)
    }
}

pub fn errptr_to_result_c<T>(p: *const T) -> Result<*const T, BchError> {
    let addr = p as usize;
    let max_err: isize = -4096;
    if addr > max_err as usize {
        Err(BchError(-(addr as i32)))
    } else {
        Ok(p)
    }
}
