use crate::c;
use core::ffi::{c_int, CStr};
use core::fmt;

pub use crate::c::bch_errcode;

// Each errcode by its BCH_ERRCODES() name: transaction_restart for
// bch_errcode::BCH_ERR_transaction_restart.
macro_rules! errcode_names {
    ($(($class:tt, $err:tt, $nr:tt)),* $(,)?) => { ::paste::paste! { $(
        #[allow(non_upper_case_globals)]
        pub const $err: bch_errcode = bch_errcode::[<BCH_ERR_ $err>];
    )* } };
}
c::BCH_ERRCODES!(errcode_names);

/// A bcachefs error code or errno, positive: errnos below BCH_ERR_START (2048),
/// bcachefs's own above. Unlike bch_errcode, a raw errno never makes an invalid
/// discriminant.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct BchError(i32);

// The bcachefs codes extend the errnos, so every errno is a BchError - as the
// bindings spell them (u32) or as libc does (i32), positive either way.

impl From<bch_errcode> for BchError {
    fn from(code: bch_errcode) -> Self { Self(code.0 as i32) }
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

    /// The plain errno this is a case of, negated: for the VFS or userspace.
    pub fn class(&self) -> c_int {
        -self.errno()
    }

    /// Static: bch2_err_str()'s strings live for the process.
    pub fn msg(&self) -> &'static str {
        unsafe { CStr::from_ptr(c::bch2_err_str(self.0).cast()) }
            .to_str()
            .unwrap_or("unknown error")
    }

    /// The parent-chain walks index bch2_errcode_parents[] unchecked - fine for
    /// codes we threw. But the tools and the DKMS module are versioned separately,
    /// and mount.bcachefs acts on the errcode a syscall returned: a newer module can
    /// name a code this build doesn't know, which mustn't abort the mount helper.
    fn known(&self) -> bool {
        self.0.unsigned_abs() < bch_errcode::BCH_ERR_MAX.0
    }

    /// Whether this is @class or derives from it; @class is an errcode or errno.
    /// An unknown code matches nothing: its parent chain can't be seen.
    pub fn matches(&self, class: impl Into<BchError>) -> bool {
        if self.0 != 0 && self.known() {
            unsafe { c::__bch2_err_matches(self.0, class.into().0) }
        } else {
            false
        }
    }

    /// The errno at the root of the parent chain; an errno is itself. An unknown
    /// code stands for itself, as it does to a caller that doesn't know bcachefs.
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

/// A failed syscall, typed. bcachefs's errcodes reach userspace intact -
/// BCH_ERR_START plus a pinned number, below MAX_ERRNO - so an io::Error off a
/// bcachefs syscall is worth converting: bch2_fs_get_tree() deliberately
/// doesn't bch2_err_class(). No errno behind it, and it wasn't a syscall: EIO,
/// which nothing matches by accident.
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

/// ret_to_c(), for the VFS: 0, or the plain errno class.
pub fn ret_to_class<E: Into<BchError>>(ret: Result<(), E>) -> c_int {
    ret.map_or_else(|e| e.into().class(), |()| 0)
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
