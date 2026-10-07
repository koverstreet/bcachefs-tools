use crate::c;
use core::ffi::{c_int, CStr};
use core::fmt;

pub use crate::c::bch_errcode;

include!(concat!(env!("OUT_DIR"), "/errcodes_gen.rs"));

/// Safe wrapper for bcachefs/errno error codes.
/// Stores the positive error code — either a standard errno (1..2047)
/// or a bcachefs-specific code (BCH_ERR_START=2048..).
///
/// Unlike `bch_errcode` (a repr(u32) enum), this never creates an invalid
/// enum discriminant from a raw errno value.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct BchError(i32);

// The bcachefs codes extend the errnos, so every errno is a BchError - as the
// bindings spell them (u32) or as libc does (i32), positive either way.

impl From<bch_errcode> for BchError {
    fn from(code: bch_errcode) -> Self { Self(code as i32) }
}

impl From<u32> for BchError {
    fn from(errno: u32) -> Self { Self(errno as i32) }
}

impl From<i32> for BchError {
    fn from(errno: i32) -> Self { Self(errno) }
}

impl BchError {
    pub fn from_raw(code: i32) -> Self { Self(code) }

    pub fn raw(&self) -> i32 { self.0 }

    /// The plain errno this is a case of, negative, for what goes back to
    /// the VFS or userspace: as bch2_err_class() - errno(), negated.
    pub fn class(&self) -> c_int {
        -self.errno()
    }

    /// Get the error message string.
    ///
    /// Returns a static string since bch2_err_str() returns strings
    /// that live for the process lifetime.
    pub fn msg(&self) -> &'static str {
        unsafe { CStr::from_ptr(c::bch2_err_str(self.0)) }
            .to_str()
            .unwrap_or("unknown error")
    }

    /// Whether this build knows what this code is.
    ///
    /// The parent-chain walks below index bch2_errcode_parents[] and BUG_ON()
    /// rather than bounds-checking, which is fine for a code we threw
    /// ourselves. It is not fine for one that arrived from outside: the tools
    /// and the kernel module are versioned separately - bcachefs ships
    /// DKMS-only - and mount.bcachefs now acts on the errcode a syscall
    /// returned. A kernel module newer than the binary asking can name a code
    /// this table has never heard of, and aborting the mount helper over it
    /// would be a poor way to find out.
    fn known(&self) -> bool {
        self.0.unsigned_abs() < bch_errcode::BCH_ERR_MAX as u32
    }

    /// Whether this error is @class, or derives from it: as bch2_err_matches().
    /// @class is anything that converts: a bcachefs error code, or an errno.
    ///
    /// An unrecognised code is not a match: we can't see its parent chain, so
    /// there is nothing to answer with but "no".
    pub fn matches(&self, class: impl Into<BchError>) -> bool {
        if self.0 != 0 && self.known() {
            unsafe { c::__bch2_err_matches(self.0, class.into().0) }
        } else {
            false
        }
    }

    /// Return the standard errno that this error maps to.
    /// bcachefs error codes (>= 2048) walk the parent chain to their
    /// root errno; standard errnos pass through unchanged.
    ///
    /// A code we don't recognise has no chain to walk, so it stands for
    /// itself - the same thing a caller that doesn't know bcachefs sees.
    pub fn errno(&self) -> i32 {
        if self.0 == 0 || !self.known() {
            self.0
        } else {
            // __bch2_err_class takes and returns negative error codes
            -unsafe { c::__bch2_err_class(-self.0) }
        }
    }
}

/// For a lookup where not finding anything is a normal answer: found() turns
/// ENOENT - and every code derived from it - into Ok(None).
pub trait Found<T> {
    fn found(self) -> Result<Option<T>, BchError>;
}

impl<T> Found<T> for Result<T, BchError> {
    fn found(self) -> Result<Option<T>, BchError> {
        match self {
            Ok(v)                           => Ok(Some(v)),
            Err(e) if e.matches(c::ENOENT)  => Ok(None),
            Err(e)                          => Err(e),
        }
    }
}

impl fmt::Display for BchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg())
    }
}

impl fmt::Debug for BchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BchError({}, {})", self.0, self)
    }
}

impl core::error::Error for BchError {}

/// A failed syscall, typed.
///
/// bcachefs's own error codes reach userspace intact: an errcode is
/// BCH_ERR_START plus a number pinned in the x-macro, which lands well below
/// MAX_ERRNO, so the syscall return path hands it back as an ordinary errno
/// and libstd stores it verbatim. That is what makes an io::Error off a
/// bcachefs syscall worth converting rather than printing - see
/// bch2_fs_get_tree(), which deliberately does not bch2_err_class().
///
/// An io::Error with no errno behind it did not come from a syscall; EIO is
/// the honest stand-in, and nothing matches it by accident.
#[cfg(feature = "std")]
impl From<std::io::Error> for BchError {
    fn from(e: std::io::Error) -> BchError {
        const EIO: i32 = 5;

        BchError::from_raw(e.raw_os_error().unwrap_or(EIO))
    }
}

/// A failed allocation is a bare -ENOMEM, as darray_push()'s is: callers that
/// want to say where map it to their own ENOMEM_* code.
impl From<crate::util::alloc::AllocError> for BchError {
    fn from(_: crate::util::alloc::AllocError) -> BchError {
        c::ENOMEM.into()
    }
}

pub fn ret_to_result(ret: c_int) -> Result<c_int, BchError> {
    if ret < 0 && ret > -4096 {
        Err(BchError(-ret))
    } else {
        Ok(ret)
    }
}

pub fn ret_to_result_void(ret: c_int) -> Result<(), BchError> {
    ret_to_result(ret).map(|_| ())
}

/// The other way: a result as C's int, 0 or -errcode - for Rust that C calls.
pub fn ret_to_c<E: Into<BchError>>(ret: Result<(), E>) -> c_int {
    match ret {
        Ok(())  => 0,
        Err(e)  => -e.into().raw(),
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
