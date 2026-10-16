// SPDX-License-Identifier: GPL-2.0

//! Module parameters defined in Rust: module_param!(), as C's
//! module_param_named() and MODULE_PARM_DESC() together.
//!
//! Not the kernel crate's (kernel::module_param): that's for its module!()
//! macro - our module's init is C - and only for parameters set at load, a
//! SetOnce with perm 0, so nothing in sysfs. Ours are C's: the kernel_param in
//! __param points at C's own param_ops_bool/uint/int, which parse, print and
//! store the value, at load and, as perm allows, through
//! /sys/module/bcachefs/parameters/ while the module is loaded.
//!
//! The value is an atomic with the C type's layout - AtomicBool is bool's,
//! AtomicU32 unsigned int's. param_ops_* store to it through kernel_param.arg
//! with plain C stores, under kernel_param_lock(); Rust loads it relaxed, as
//! C reads a module parameter: an untorn read of a value that may change
//! underneath, ordered against nothing.
//!
//! Loadable only, as bcachefs is (DKMS): the name has no "bcachefs." prefix,
//! and kernel_param.mod is __this_module, which only a module has.
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
        perm = $perm:expr, desc = $desc:literal $(;)?
    ) => {
        $(#[$attr])*
        #[allow(non_upper_case_globals)]
        $vis static $name: $crate::util::module_param::ModuleParam<$t> =
            $crate::util::module_param::ModuleParam::<$t>::new($default);

        #[cfg(kernel)]
        $(#[$reg])*
        const _: () = {
            use $crate::util::module_param::{modinfo, modinfo_len, verify_perm, KernelParam,
                                             ParamType};

            #[link_section = "__param"]
            #[used(compiler)]
            static PARAM: KernelParam = KernelParam(::kernel::bindings::kernel_param {
                name:  ::core::concat!(::core::stringify!($name), "\0").as_ptr().cast(),
                mod_:  &raw mut ::kernel::bindings::__this_module,
                ops:   <$t as ParamType>::OPS,
                perm:  verify_perm($perm),
                level: -1,
                flags: 0,
                __bindgen_anon_1: ::kernel::bindings::kernel_param__bindgen_ty_1 {
                    arg: $name.as_arg(),
                },
            });

            const PARMTYPE: &[&str] =
                &["parmtype=", ::core::stringify!($name), ":", <$t as ParamType>::C_NAME];
            #[link_section = ".modinfo"]
            #[used(compiler)]
            static PARMTYPE_INFO: [u8; modinfo_len(PARMTYPE)] = modinfo(PARMTYPE);

            const PARM: &[&str] = &["parm=", ::core::stringify!($name), ":", $desc];
            #[link_section = ".modinfo"]
            #[used(compiler)]
            static PARM_INFO: [u8; modinfo_len(PARM)] = modinfo(PARM);
        };
    };
}
