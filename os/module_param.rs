// SPDX-License-Identifier: GPL-2.0

//! Module parameters defined in Rust: module_param!(), as C's
//! module_param_named() and MODULE_PARM_DESC() together.
//!
//! Not the kernel crate's (kernel::module_param): that's for its module!()
//! macro - our module's init is our own, init/module.rs - and only for
//! parameters set at load, a SetOnce with perm 0, so nothing in sysfs. Ours
//! are C's: the kernel_param in __param points at C's own
//! param_ops_bool/uint/int, which parse, print and store the value, at load
//! and, as perm allows, through /sys/module/bcachefs/parameters/ while the
//! module is loaded.
//!
//! The value is an atomic with the C type's layout - AtomicBool is bool's,
//! AtomicU32 unsigned int's. param_ops_* store to it through kernel_param.arg
//! with plain C stores, under kernel_param_lock(); Rust loads it relaxed, as
//! C reads a module parameter: an untorn read of a value that may change
//! underneath, ordered against nothing.
//!
//! Loadable, as bcachefs ships (DKMS), or built in - cfg(MODULE), which kbuild
//! sets for a module's objects. Built in, as C's moduleparam.h has it, there
//! is no __this_module for kernel_param.mod, and the name and the .modinfo
//! strings are prefixed "bcachefs.": MODULE_PARAM_PREFIX and
//! __MODULE_INFO_PREFIX.
//!
//! The registration and the .modinfo strings are #[used(compiler)], as the
//! kernel crate's are: plain #[used] is used(linker) from Rust 1.89, and the
//! retain flag that sets gives each static its own section - which modpost,
//! expecting one .modinfo, doesn't read. used_with_arg is in the features
//! kbuild allows non-rust/ code from 6.16, our oldest kernel.
//!
//! Userspace has no module parameters: there the static is the value, always
//! its default.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

/// A type a module parameter can have: one C's param_ops_* handle.
pub trait ParamType: Copy {
    /// The value's storage: the C type's layout, which param_ops_* store to.
    type Value: Sync;
    /// The C type, as module_param() takes it and modinfo shows it.
    const C_NAME: &'static str;
    /// C's ops for it.
    #[cfg(kernel)]
    const OPS: *const kernel::bindings::kernel_param_ops;

    fn load(v: &Self::Value) -> Self;
}

/// A module parameter's value: the static module_param!() defines.
#[repr(transparent)]
pub struct ModuleParam<T: ParamType>(T::Value);

impl<T: ParamType> ModuleParam<T> {
    pub fn get(&self) -> T {
        T::load(&self.0)
    }

    /// Where param_ops_* store: the kernel_param's arg.
    #[doc(hidden)]
    pub const fn as_arg(&self) -> *mut core::ffi::c_void {
        (&raw const self.0).cast_mut().cast()
    }
}

macro_rules! param_types {
    ($($t:ty: $value:ty, $c_name:literal, $ops:ident;)*) => {$(
        impl ParamType for $t {
            type Value = $value;
            const C_NAME: &'static str = $c_name;
            #[cfg(kernel)]
            const OPS: *const kernel::bindings::kernel_param_ops =
                &raw const kernel::bindings::$ops;

            fn load(v: &$value) -> Self {
                v.load(Ordering::Relaxed)
            }
        }

        impl ModuleParam<$t> {
            pub const fn new(v: $t) -> Self {
                Self(<$value>::new(v))
            }
        }
    )*};
}

param_types! {
    bool: AtomicBool, "bool", param_ops_bool;
    u32:  AtomicU32,  "uint", param_ops_uint;
    i32:  AtomicI32,  "int",  param_ops_int;
}

/// kernel_param, which holds raw pointers, made Sync for a static: only the
/// module loader and sysfs touch it, under kernel_param_lock().
#[cfg(kernel)]
#[doc(hidden)]
#[repr(transparent)]
pub struct KernelParam(pub kernel::bindings::kernel_param);

#[cfg(kernel)]
unsafe impl Sync for KernelParam {}

/// @perm, checked as C's VERIFY_OCTAL_PERMISSIONS() does - at compile time,
/// in the static's initializer.
#[doc(hidden)]
pub const fn verify_perm(perm: u16) -> u16 {
    let (u, g, o) = ((perm >> 6) & 7, (perm >> 3) & 7, perm & 7);
    assert!(perm <= 0o777, "module parameter permissions out of range");
    assert!(u & 4 >= g & 4 && g & 4 >= o & 4,
            "module parameter readable by group or other, but not user or group");
    assert!(u & 2 >= g & 2, "module parameter writable by group, but not user");
    assert!(o & 2 == 0, "module parameter writable by other");
    perm
}

/// The length of the NUL-terminated concatenation of @parts.
#[doc(hidden)]
pub const fn modinfo_len(parts: &[&str]) -> usize {
    let mut len = 1;
    let mut i = 0;
    while i < parts.len() {
        len += parts[i].len();
        i += 1;
    }
    len
}

/// A .modinfo string: @parts concatenated, NUL-terminated; LEN is
/// modinfo_len() of them.
#[doc(hidden)]
pub const fn modinfo<const LEN: usize>(parts: &[&str]) -> [u8; LEN] {
    let mut out = [0u8; LEN];
    let mut at = 0;
    let mut i = 0;
    while i < parts.len() {
        let b = parts[i].as_bytes();
        let mut j = 0;
        while j < b.len() {
            assert!(b[j] != 0, "modinfo string contains a NUL byte");
            out[at] = b[j];
            at += 1;
            j += 1;
        }
        i += 1;
    }
    out
}

/// What C's __MODULE_INFO_PREFIX puts before a .modinfo tag: nothing in a
/// module, the module's name built in.
#[cfg(all(kernel, MODULE))]
pub const MODINFO_PREFIX: &str = "";
#[cfg(all(kernel, not(MODULE)))]
pub const MODINFO_PREFIX: &str = "bcachefs.";

/// A module parameter, as C's module_param_named() and MODULE_PARM_DESC():
///
/// ```text
/// module_param! {
///     pub(crate) static snapshot_delete_bail_before_inodes: bool = false;
///     #[cfg(CONFIG_BCACHEFS_DEBUG)]
///     perm = 0o644, desc = "Cancel snapshot deletion before the inode keys are deleted";
/// }
/// ```
///
/// The static is a ModuleParam, read with get(), under the parameter's name;
/// attributes before `perm` gate the parameter's registration, not the static,
/// so a debug-only parameter reads as its default in other builds, as C's
/// #ifdef'd module_param() leaves its variable.
#[macro_export]
macro_rules! module_param {
    (
        $(#[$attr:meta])*
        $vis:vis static $name:ident: $t:ty = $default:expr;
        $(#[$reg:meta])*
        perm = $perm:expr $(, desc = $desc:literal)? $(;)?
    ) => {
        $(#[$attr])*
        #[allow(non_upper_case_globals)]
        $vis static $name: $crate::os::module_param::ModuleParam<$t> =
            $crate::os::module_param::ModuleParam::<$t>::new($default);

        #[cfg(kernel)]
        $(#[$reg])*
        $crate::module_param_cb!($name, $name.as_arg(),
                                 <$t as $crate::os::module_param::ParamType>::OPS,
                                 <$t as $crate::os::module_param::ParamType>::C_NAME,
                                 $perm $(, $desc)?);
    };
}

/// A static key, a StaticKeyFalse, as a bool module parameter @name, perm
/// 0644: set, it's enabled or disabled; read, it's Y or N.
#[cfg(kernel)]
#[macro_export]
macro_rules! module_param_static_key {
    ($name:ident, $key:path, $desc:expr) => {
        $crate::module_param_cb!($name, $crate::os::jump_label::StaticKeyFalse::as_ptr(&$key).cast(),
                                 &$crate::os::module_param::STATIC_KEY_OPS, "static_key_t", 0o644, $desc);
    };
}

#[cfg(kernel)]
#[doc(hidden)]
pub use static_key::OPS as STATIC_KEY_OPS;

#[cfg(kernel)]
mod static_key {
    use ::kernel::bindings::{kernel_param, kernel_param_ops, param_set_bool};
    use crate::os::jump_label::StaticKeyFalse;
    use crate::util::ffi::c_char;
    use core::ffi::c_int;
    use core::sync::atomic::{AtomicI32, Ordering::Relaxed};

    /// # Safety
    /// @kp is a parameter module_param_static_key!() registered: its arg is
    /// a StaticKeyFalse, a static.
    unsafe fn key(kp: *const kernel_param) -> &'static StaticKeyFalse {
        unsafe { &*(*kp).__bindgen_anon_1.arg.cast::<StaticKeyFalse>() }
    }

    /// Its count, an atomic_t.
    fn enabled(key: &StaticKeyFalse) -> &AtomicI32 {
        unsafe { AtomicI32::from_ptr(&raw mut (*key.as_ptr()).key.enabled.counter) }
    }

    #[cfg(CONFIG_JUMP_LABEL)]
    fn set_key(key: &StaticKeyFalse, v: bool) {
        unsafe {
            let k = &raw mut (*key.as_ptr()).key;
            if v {
                ::kernel::bindings::static_key_enable(k)
            } else {
                ::kernel::bindings::static_key_disable(k)
            }
        }
    }

    #[cfg(not(CONFIG_JUMP_LABEL))]
    fn set_key(key: &StaticKeyFalse, v: bool) {
        let (to, from) = (i32::from(v), i32::from(!v));
        let n = enabled(key).load(Relaxed);
        if n != from {
            crate::warn_on_once!(n != to);
            return;
        }
        enabled(key).store(to, Relaxed);
    }

    unsafe extern "C" fn set(val: *const c_char, kp: *const kernel_param) -> c_int {
        /* Match bool exactly, by re-using it. */
        let key = unsafe { key(kp) };
        let mut boolkp = unsafe { kp.read() };
        let mut v = false;

        boolkp.__bindgen_anon_1.arg = (&raw mut v).cast();

        let ret = unsafe { param_set_bool(val, &boolkp) };
        if ret != 0 {
            return ret;
        }
        set_key(key, v);
        0
    }

    unsafe extern "C" fn get(buffer: *mut c_char, kp: *const kernel_param) -> c_int {
        let s = if enabled(unsafe { key(kp) }).load(Relaxed) > 0 { b"Y\n\0" } else { b"N\n\0" };
        unsafe { buffer.copy_from_nonoverlapping(s.as_ptr().cast(), s.len()) };
        2
    }

    pub static OPS: kernel_param_ops = kernel_param_ops {
        flags: ::kernel::bindings::KERNEL_PARAM_OPS_FL_NOARG,
        set:   Some(set),
        get:   Some(get),
        free:  None,
    };
}

/// A module parameter's registration, as C's module_param_cb(),
/// __MODULE_PARM_TYPE() and MODULE_PARM_DESC(): @name, its value at @arg, read
/// and written by @ops, shown in modinfo as of type @c_type.
#[cfg(kernel)]
#[macro_export]
macro_rules! module_param_cb {
    ($name:ident, $arg:expr, $ops:expr, $c_type:expr, $perm:expr $(, $desc:expr)?) => {
        $crate::module_info!("parmtype" = ::core::stringify!($name), ":", $c_type);
        $($crate::module_info!("parm" = ::core::stringify!($name), ":", $desc);)?

        const _: () = {
            use $crate::os::module_param::{verify_perm, KernelParam};

            #[unsafe(link_section = "__param")]
            #[used(compiler)]
            static PARAM: KernelParam = KernelParam(::kernel::bindings::kernel_param {
                #[cfg(MODULE)]
                name:  ::core::concat!(::core::stringify!($name), "\0").as_ptr().cast(),
                #[cfg(not(MODULE))]
                name:  ::core::concat!("bcachefs.", ::core::stringify!($name), "\0").as_ptr().cast(),
                #[cfg(MODULE)]
                mod_:  &raw mut ::kernel::bindings::__this_module,
                #[cfg(not(MODULE))]
                mod_:  ::core::ptr::null_mut(),
                ops:   $ops,
                perm:  verify_perm($perm),
                level: -1,
                flags: 0,
                __bindgen_anon_1: ::kernel::bindings::kernel_param__bindgen_ty_1 {
                    arg: $arg,
                },
            });
        };
    };
}

/// A .modinfo string, as C's MODULE_INFO(): @tag=, then @parts.
#[cfg(kernel)]
#[macro_export]
macro_rules! module_info {
    ($tag:literal = $($part:expr),+) => {
        const _: () = {
            use $crate::os::module_param::{modinfo, modinfo_len, MODINFO_PREFIX};

            const INFO: &[&str] = &[MODINFO_PREFIX, $tag, "=", $($part),+];
            #[unsafe(link_section = ".modinfo")]
            #[used(compiler)]
            static INFO_STR: [u8; modinfo_len(INFO)] = modinfo(INFO);
        };
    };
}
