//! bcachefs error codes.
//!
//! The errcodes extend the errnos: BCH_ERRCODES() numbers bcachefs's own from
//! BCH_ERR_START (2048) up, each with a parent - an errno, another errcode, or
//! none. A code is thus a chain: ENOENT_inode -> ENOENT, and
//! transaction_restart_relock -> transaction_restart. matches(class) asks
//! whether @class is on the chain, errno() is its root: what the VFS and
//! userspace see.
//!
//! ERRCODES, generated from BCH_ERRCODES(), holds each errcode's name and
//! parent; errcode.c keeps C's own copy for its C callers. Parents are written
//! as an errno or BCH_ERR_<name>, and `class` resolves both to numbers - the
//! errnos from the kernel's bindings, or linux-raw-sys's in userspace. An
//! errno's name is errname()'s, strerror()'s in userspace.
//!
//! A number with no entry has no parent and stands for itself. That includes
//! codes past BCH_ERR_MAX: the tools and the DKMS module are versioned
//! separately, and mount.bcachefs acts on the errcode a syscall returned, so a
//! newer module can return one this build doesn't know.

use crate::c;
use core::ffi::{c_int, CStr};
use core::fmt;
use core::iter;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI32, Ordering};

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

const START: u32 = bch_errcode::BCH_ERR_START.0;
const N: usize = (bch_errcode::BCH_ERR_MAX.0 - START) as usize;

#[allow(dead_code, non_upper_case_globals)]
mod class {
    #[cfg(kernel)]
    pub(super) use crate::c::*;
    #[cfg(not(kernel))]
    pub(super) use linux_raw_sys::errno::*;

    macro_rules! codes {
        ($(($class:tt, $err:tt, $nr:tt)),* $(,)?) => { ::paste::paste! { $(
            pub(super) const [<BCH_ERR_ $err>]: u32 = super::bch_errcode::[<BCH_ERR_ $err>].0;
        )* } };
    }
    crate::c::BCH_ERRCODES!(codes);
}

macro_rules! class_of {
    (0) => { 0 };
    ($class:ident) => { class::$class as u32 };
}

macro_rules! errcode_table {
    ($(($class:tt, $err:tt, $nr:tt)),* $(,)?) => {
        static ERRCODES: [Option<(&str, u32)>; N] = {
            let mut t = [None; N];
            $(t[$nr] = Some((stringify!($err), class_of!($class)));)*
            t
        };
    };
}
c::BCH_ERRCODES!(errcode_table);

fn errcode(err: u32) -> Option<(&'static str, u32)> {
    ERRCODES.get(err.checked_sub(START)? as usize).copied().flatten()
}

fn errname(err: u32) -> Option<&'static str> {
    #[cfg(kernel)]
    let p = unsafe { c::errname(err as c_int) };
    #[cfg(not(kernel))]
    let p = {
        unsafe extern "C" { fn strerror(err: c_int) -> *const core::ffi::c_char; }
        unsafe { strerror(err as c_int) }
    };
    // SAFETY: a static string, or NULL
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p.cast()) }).and_then(|s| s.to_str().ok())
}

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

    pub fn msg(&self) -> &'static str {
        let name = match self.0.unsigned_abs() {
            0              => Some("(No error)"),
            e if e < START => errname(e),
            e              => errcode(e).map(|(name, _)| name),
        };
        name.unwrap_or("(Invalid error)")
    }

    fn chain(&self) -> impl Iterator<Item = u32> {
        iter::successors(Some(self.0.unsigned_abs()), |&e| {
            errcode(e).map(|(_, parent)| parent).filter(|&p| p != 0)
        })
    }

    pub fn matches(&self, class: impl Into<BchError>) -> bool {
        let class = class.into().0.unsigned_abs();
        self.0 != 0 && self.chain().any(|e| e == class)
    }

    pub fn errno(&self) -> i32 {
        self.chain().last().unwrap() as i32
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

/// What can't fail - an infallible initializer, where a fallible one goes.
impl From<core::convert::Infallible> for BchError {
    fn from(e: core::convert::Infallible) -> BchError {
        match e {}
    }
}

/// A failed allocation is a bare -ENOMEM, as darray_push()'s is: callers that
/// want to say where map it to their own ENOMEM_* code.
impl From<crate::util::alloc::AllocError> for BchError {
    fn from(_: crate::util::alloc::AllocError) -> BchError {
        c::ENOMEM.into()
    }
}

/// The kernel crate's errors, a negative errno: from its APIs - uaccess's
/// EFAULT, say.
#[cfg(kernel)]
impl From<kernel::error::Error> for BchError {
    fn from(e: kernel::error::Error) -> BchError {
        BchError(-e.to_errno())
    }
}

/// An error set from anywhere, once: the first set is kept, later ones
/// dropped - why something was cancelled, or what a fork-join's jobs ran
/// into first.
pub struct FirstErr(AtomicI32);

impl FirstErr {
    pub const fn new() -> Self {
        FirstErr(AtomicI32::new(0))
    }

    /// @e, unless there's one already.
    pub fn set(&self, e: impl Into<BchError>) {
        let _ = self.0.compare_exchange(0, e.into().raw(), Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn get(&self) -> Option<BchError> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            e => Some(BchError(e)),
        }
    }

    pub fn result(&self) -> Result<(), BchError> {
        self.get().map_or(Ok(()), Err)
    }
}

impl Default for FirstErr {
    fn default() -> Self {
        Self::new()
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

/// A pointer from C that's an ERR_PTR() on failure, and never NULL: its
/// error, or it. An ERR_PTR() is a -errno as a pointer - one of the top
/// MAX_ERRNO (4095) addresses.
pub fn errptr_to_result<T>(p: NonNull<T>) -> Result<NonNull<T>, BchError> {
    let addr = p.as_ptr() as usize;
    if addr > usize::MAX - 4095 {
        Err(BchError(-(addr as i32)))
    } else {
        Ok(p)
    }
}

/// One that may be NULL too, as IS_ERR_OR_NULL() tests: None for NULL.
pub fn errptr_or_null<T>(p: *mut T) -> Result<Option<NonNull<T>>, BchError> {
    NonNull::new(p).map(errptr_to_result).transpose()
}

/// ERR_PTR(): @e as the pointer C returns for it.
pub fn err_ptr<T>(e: BchError) -> *mut T {
    (-(e.raw() as isize)) as *mut T
}
