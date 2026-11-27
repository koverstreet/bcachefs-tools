// SPDX-License-Identifier: GPL-2.0

//! The kernel's operation tables - file_operations, and the VFS's super,
//! inode and export operations - as traits, each from one declaration:
//! vfs_ops!.
//!
//! The header names the trait, the C table, and the vtable that fills it in,
//! `$VTable<P: Bound> = Impl`: generic over P, what the kernel object's
//! private pointer points to, Impl the implementor had from it - P itself
//! for the VFS's, P::Ops for a file's. After it, `type Foo: Bound;` is an
//! associated type, what the filesystem's types are - its inode's, say -
//! which ops name as Self::Foo; then `const FOO: T = default;`, which an
//! impl may set, for the header's `|ops| { }` to read.
//!
//! An op is its Rust signature. From it come the trait method - unreachable
//! unless given a body; #[vtable] sets HAS_* for what an impl has - the
//! extern "C" adapter, and the table, OPS, then whatever the header's
//! `|ops| { }` sets.
//!
//! The adapter takes each argument as the kernel passes its Rust type -
//! FromRaw::Raw, one raw type to each - and converts it; the return goes
//! through IntoRaw to whatever the table's field has, as Result<(),
//! BchError> is an int to one op and an ERR_PTR to another. Where the raw
//! arguments don't map one to one, the adapter is written out, after `=>
//! extern`.
//!
//! `&self = arg` makes an op a method, &self had from arg by SelfOf. Ops that
//! dereference raw pointers go in `unsafe { }`, their contract the kernel's.
//!
//! FromRaw's impls say once a type what the kernel guarantees of what it
//! passes: referenced, and ours, for the call. IntoRaw's class errors.

// The tools' file ops are open and read, their adapters written out: none of
// the converting is used there
#![cfg_attr(not(kernel), allow(dead_code, unused_imports))]


use crate::errcode::{ret_to_class, BchError};

/// A raw argument an op is passed, as its trait method takes it: each Rust
/// type from the one raw type the kernel passes it as.
pub(crate) trait FromRaw: Sized {
    type Raw;

    /// # Safety
    /// @r is what the kernel passes: live, and ours, for the call.
    unsafe fn from_raw(r: Self::Raw) -> Self;
}

/// A trait method's return, as the op returns it.
pub(crate) trait IntoRaw<R> {
    fn into_raw(self) -> R;
}

impl<T> IntoRaw<T> for T {
    fn into_raw(self) -> T { self }
}

/// What &self is had from, for a `&self = arg` op of a vtable over P.
pub(crate) trait SelfOf<P> {
    type Out;

    /// # Safety
    /// As FromRaw's, and its private pointer a P's.
    unsafe fn self_of<'a>(&self) -> &'a Self::Out;
}

/// FromRaw impls, one a line: the Rust type - its generics, if it has any,
/// after it in [] - the raw one, and the one had from the other, under
/// FromRaw's contract. 'a is every impl's.
macro_rules! from_raw {
    ($($(#[doc = $d:expr])* $t:ty $([$($g:tt)*])? = $raw:ty => |$r:ident| $e:expr;)*) => {$(
        $(#[doc = $d])*
        impl<'a, $($($g)*)?> $crate::os::ops::FromRaw for $t {
            type Raw = $raw;
            #[allow(unused_unsafe)]
            unsafe fn from_raw($r: $raw) -> Self { unsafe { $e } }
        }
    )*};
}
pub(crate) use from_raw;

/// IntoRaw impls, as from_raw!'s.
macro_rules! into_raw {
    ($($(#[doc = $d:expr])* $t:ty => $raw:ty: |$v:ident| $e:expr;)*) => {$(
        $(#[doc = $d])*
        impl $crate::os::ops::IntoRaw<$raw> for $t {
            fn into_raw(self) -> $raw { let $v = self; $e }
        }
    )*};
}
pub(crate) use into_raw;

// What's passed as it's taken: the scalars, and raw pointers
macro_rules! as_passed {
    ($($t:ty),*) => {$( impl FromRaw for $t { type Raw = $t; unsafe fn from_raw(r: $t) -> $t { r } } )*};
}
as_passed!(bool, u16, u32, u64, usize, i32, i64);

from_raw! {
    *mut T [T]   = *mut T   => |r| r;
    *const T [T] = *const T => |r| r;
}

into_raw! {
    Result<(), BchError> => i32: |r| ret_to_class(r);
    /// A position: llseek's.
    Result<u64, BchError> => i64: |r| r.map_or_else(|e| e.class().into(), |pos| pos as i64);
}

// The kernel's long, isize: fallocate's
#[cfg(kernel)]
into_raw! {
    Result<(), BchError> => crate::util::ffi::c_long: |r| ret_to_class(r) as crate::util::ffi::c_long;
}

#[cfg(kernel)]
use {core::ffi::CStr, crate::c, crate::util::ffi::c_char};

#[cfg(kernel)]
from_raw! {
    &'a CStr              = *const c_char       => |r| CStr::from_ptr(r.cast());
    &'a mut c::kstat      = *mut c::kstat       => |r| &mut *r;
    &'a mut c::iattr      = *mut c::iattr       => |r| &mut *r;
    &'a mut c::file_kattr = *mut c::file_kattr  => |r| &mut *r;
    &'a mut c::delayed_call = *mut c::delayed_call => |r| &mut *r;
    crate::posix::mode::umode_t = c::umode_t    => |r| crate::posix::mode::umode_t(r);
    crate::posix::dev::dev_t   = c::dev_t      => |r| crate::os::kernel::dev_from_kernel(r);
    /// The kernel crate's is fs.h's declaration, opaque; codegen's the same.
    &'a mut c::kstatfs    = *mut kernel::bindings::kstatfs => |r| &mut *r.cast();
}

// quotactl's
#[cfg(kernel)]
from_raw! {
    c::kqid               = c::kqid             => |r| r;
    &'a mut c::kqid       = *mut c::kqid        => |r| &mut *r;
    &'a mut c::qc_state   = *mut c::qc_state    => |r| &mut *r;
    &'a c::qc_info        = *mut c::qc_info     => |r| &*r;
    &'a c::qc_dqblk       = *mut c::qc_dqblk    => |r| &*r;
    &'a mut c::qc_dqblk   = *mut c::qc_dqblk    => |r| &mut *r;
}

/// @e, classed, as an ERR_PTR().
pub(crate) fn err_class_ptr<T>(e: BchError) -> *mut T {
    e.class() as isize as *mut T
}

#[cfg(kernel)]
into_raw! {
    /// mkdir's: NULL for the dentry it was given, instantiated.
    Result<(), BchError>    => *mut c::dentry: |r| r.map_or_else(err_class_ptr, |()| core::ptr::null_mut());
    Result<usize, BchError> => isize: |r| r.map_or_else(|e| e.class() as isize, |n| n as isize);
}

/// See the file comment.
#[macro_export]
macro_rules! vfs_ops {
    (
        $(#[$tm:meta])*
        $vis:vis trait $Trait:ident $(: $sup:ident $(+ $sups:ident)*)? for $Ops:ty,
            $VTable:ident<$P:ident: $Bound:path> = $Impl:ty $(, |$o:ident| $init:block)?;
        $( type $Assoc:ident $(: $AssocBound:path)?; )*
        $( const $Const:ident: $ConstTy:ty = $ConstV:expr; )*
        $( $(#[doc = $doc:expr])* $(#[cfg($cfg:meta)])*
           fn $name:ident ( $(&self = $sa:ident ,)? $($arg:ident : $ty:ty),* $(,)? )
              $(-> $ret:ty)? $({ $($body:tt)* })?
              $(=> extern ( $($xarg:ident : $xty:ty),* ) $(-> $xret:ty)? { $($xbody:tt)* })? ; )*
        $( unsafe {
            $( $(#[doc = $udoc:expr])* $(#[cfg($ucfg:meta)])*
               fn $uname:ident ( $(&self = $usa:ident ,)? $($uarg:ident : $uty:ty),* $(,)? )
                  $(-> $uret:ty)? $({ $($ubody:tt)* })? ; )*
        } )?
    ) => { $crate::paste! {
        $(#[$tm])*
        $vis trait $Trait: Sized + 'static $(+ $sup $(+ $sups)*)? {
            $( type $Assoc $(: $AssocBound)?; )*
            $( const $Const: $ConstTy = $ConstV; )*
            $(
                $(#[cfg($cfg)])*
                const [<HAS_ $name:upper>]: bool = $crate::vfs_ops!(@has $({ $($body)* })?);
                $crate::vfs_ops!(@method [$(#[doc = $doc])* $(#[cfg($cfg)])*] [] [$($sa)?]
                                 $name [$($arg: $ty),*] [$($ret)?] [$($($body)*)?]);
            )*
            $($(
                $(#[cfg($ucfg)])*
                const [<HAS_ $uname:upper>]: bool = $crate::vfs_ops!(@has $({ $($ubody)* })?);
                $crate::vfs_ops!(@method [$(#[doc = $udoc])* $(#[cfg($ucfg)])*] [unsafe] [$($usa)?]
                                 $uname [$($uarg: $uty),*] [$($uret)?] [$($($ubody)*)?]);
            )*)?
        }

        // The adapters are Impl's - in an impl for it of a trait of their own -
        // so that an op's types can name its associated types, Self::Foo
        #[doc(hidden)]
        $vis trait [<$Trait Adapters>]<$P: $Bound>: $Trait {
            $(
                $crate::vfs_ops!(@adapter_fn decl [$(#[cfg($cfg)])*] [$Trait $P $Impl] [$($sa)?]
                                 $name [<raw_ $name>] [$($arg: $ty),*] [$($ret)?]
                                 [$( ($($xarg: $xty),*) [$($xret)?] { $($xbody)* } )?]);
            )*
            $($(
                $crate::vfs_ops!(@adapter_fn decl [$(#[cfg($ucfg)])*] [$Trait $P $Impl] [$($usa)?]
                                 $uname [<raw_ $uname>] [$($uarg: $uty),*] [$($uret)?] []);
            )*)?
        }

        impl<$P: $Bound> [<$Trait Adapters>]<$P> for $Impl {
            $(
                $crate::vfs_ops!(@adapter_fn def [$(#[cfg($cfg)])*] [$Trait $P $Impl] [$($sa)?]
                                 $name [<raw_ $name>] [$($arg: $ty),*] [$($ret)?]
                                 [$( ($($xarg: $xty),*) [$($xret)?] { $($xbody)* } )?]);
            )*
            $($(
                $crate::vfs_ops!(@adapter_fn def [$(#[cfg($ucfg)])*] [$Trait $P $Impl] [$($usa)?]
                                 $uname [<raw_ $uname>] [$($uarg: $uty),*] [$($uret)?] []);
            )*)?
        }

        $vis struct $VTable<$P>(::core::marker::PhantomData<$P>);

        impl<$P: $Bound> $VTable<$P> {
            $vis const OPS: $Ops = {
                // zero is NULL, "not implemented"
                let mut ops: $Ops = ::pin_init::zeroed();
                $( $(#[cfg($cfg)])* if <$Impl>::[<HAS_ $name:upper>] {
                    ops.$name = Some(<$Impl as [<$Trait Adapters>]<$P>>::[<raw_ $name>]);
                } )*
                $($( $(#[cfg($ucfg)])* if <$Impl>::[<HAS_ $uname:upper>] {
                    ops.$uname = Some(<$Impl as [<$Trait Adapters>]<$P>>::[<raw_ $uname>]);
                } )*)?
                $( let ops = { let mut $o = ops; $init $o }; )?
                ops
            };

            /// OPS as the static C reads: a C ops struct, fn and static
            /// pointers that nothing writes through - $P's.
            $vis const STATIC: $crate::types::CStatic<$Ops, $P> = unsafe { $crate::types::CStatic::new(Self::OPS) };
        }
    } };

    (@has { $($body:tt)* }) => { true };
    (@has) => { false };

    // The trait method: &self for a `&self = arg` op; the body given, or one
    // that can't be reached - it's not in the vtable
    (@method [$($attr:tt)*] [$($u:ident)?] [$sa:ident] $name:ident [$($arg:ident: $ty:ty),*]
             [$($ret:ty)?] [$($body:tt)*]) => {
        $($attr)* $($u)? fn $name(&self, $($arg: $ty),*) $(-> $ret)? {
            $crate::vfs_ops!(@body [$($arg)*] $($body)*)
        }
    };
    (@method [$($attr:tt)*] [$($u:ident)?] [] $name:ident [$($arg:ident: $ty:ty),*]
             [$($ret:ty)?] [$($body:tt)*]) => {
        $($attr)* $($u)? fn $name($($arg: $ty),*) $(-> $ret)? {
            $crate::vfs_ops!(@body [$($arg)*] $($body)*)
        }
    };
    (@body [$($arg:ident)*]) => {{
        $(let _ = $arg;)*
        unreachable!("not in the vtable")
    }};
    (@body [$($arg:ident)*] $($body:tt)+) => {{ $($body)+ }};

    // The adapter: written out; or each argument taken as the kernel passes
    // its type, and converted, the method called - on &self, had from the
    // argument while it's still raw - and the return converted to what the
    // table's field has
    (@adapter_fn decl [$($cfg:tt)*] $tpi:tt $sa:tt $name:ident $raw:ident $args:tt $ret:tt
                 [($($xarg:ident: $xty:ty),*) [$($xret:ty)?] { $($xbody:tt)* }]) => {
        $($cfg)*
        unsafe extern "C" fn $raw($($xarg: $xty),*) $(-> $xret)?;
    };
    (@adapter_fn def [$($cfg:tt)*] $tpi:tt $sa:tt $name:ident $raw:ident $args:tt $ret:tt
                 [($($xarg:ident: $xty:ty),*) [$($xret:ty)?] { $($xbody:tt)* }]) => {
        $($cfg)*
        unsafe extern "C" fn $raw($($xarg: $xty),*) $(-> $xret)? { $($xbody)* }
    };
    (@adapter_fn $mode:ident $cfg:tt $tpi:tt $sa:tt $name:ident $raw:ident $args:tt [] []) => {
        $crate::vfs_ops!(@adapter_fn $mode $cfg $tpi $sa $name $raw $args [()] []);
    };
    (@adapter_fn decl [$($cfg:tt)*] $tpi:tt $sa:tt $name:ident $raw:ident
                 [$($arg:ident: $ty:ty),*] [$ret:ty] []) => {
        $($cfg)*
        unsafe extern "C" fn $raw<RawRet>($($arg: <$ty as $crate::os::ops::FromRaw>::Raw),*) -> RawRet
        where $ret: $crate::os::ops::IntoRaw<RawRet>;
    };
    (@adapter_fn def [$($cfg:tt)*] [$Trait:ident $P:ident $Impl:ty] [$($sa:ident)?] $name:ident $raw:ident
                 [$($arg:ident: $ty:ty),*] [$ret:ty] []) => {
        $($cfg)*
        unsafe extern "C" fn $raw<RawRet>($($arg: <$ty as $crate::os::ops::FromRaw>::Raw),*) -> RawRet
        where $ret: $crate::os::ops::IntoRaw<RawRet>
        {
            $crate::vfs_ops!(@call [$Trait $P $Impl] [$($sa)?] $name [$($arg: $ty),*])
        }
    };
    (@call [$Trait:ident $P:ident $Impl:ty] [$sa:ident] $name:ident [$($arg:ident: $ty:ty),*]) => {
        // SAFETY: FromRaw's and SelfOf's
        unsafe {
            let this: &$Impl = $crate::os::ops::SelfOf::<$P>::self_of(&$sa);
            $(let $arg: $ty = $crate::os::ops::FromRaw::from_raw($arg);)*
            $crate::os::ops::IntoRaw::into_raw(<$Impl as $Trait>::$name(this, $($arg),*))
        }
    };
    (@call [$Trait:ident $P:ident $Impl:ty] [] $name:ident [$($arg:ident: $ty:ty),*]) => {
        // SAFETY: FromRaw's
        unsafe {
            $(let $arg: $ty = $crate::os::ops::FromRaw::from_raw($arg);)*
            $crate::os::ops::IntoRaw::into_raw(<$Impl as $Trait>::$name($($arg),*))
        }
    };
}
