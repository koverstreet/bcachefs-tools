// SPDX-License-Identifier: GPL-2.0

//! The kernel's operation tables - file_operations, and the VFS's super,
//! inode and export operations - as traits, each from one declaration:
//! vfs_ops!.
//!
//! The header names the trait, the C table, and the vtable that fills it in,
//! `$VTable<P: Bound> = Impl`: generic over P, what the kernel object's
//! private pointer points to, Impl the implementor had from it - P itself
//! for the VFS's, P::Ops for a file's.
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

use core::ffi::c_int;

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

/// FromRaw impls, one a line: the Rust type, the raw one, and the one had
/// from the other, under FromRaw's contract.
macro_rules! from_raw {
    ($($(#[doc = $d:expr])* $t:ty = $raw:ty => |$r:ident| $e:expr;)*) => {$(
        $(#[doc = $d])*
        impl<'a> $crate::util::ops::FromRaw for $t {
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
        impl $crate::util::ops::IntoRaw<$raw> for $t {
            fn into_raw(self) -> $raw { let $v = self; $e }
        }
    )*};
}

// What's passed as it's taken: the scalars, and raw pointers
macro_rules! as_passed {
    ($($t:ty),*) => {$( impl FromRaw for $t { type Raw = $t; unsafe fn from_raw(r: $t) -> $t { r } } )*};
}
as_passed!(bool, u16, u32, u64, usize, i32, i64);

impl<T> FromRaw for *mut T {
    type Raw = *mut T;
    unsafe fn from_raw(r: *mut T) -> *mut T { r }
}

impl<T> FromRaw for *const T {
    type Raw = *const T;
    unsafe fn from_raw(r: *const T) -> *const T { r }
}

into_raw! {
    Result<(), BchError> => c_int: |r| ret_to_class(r);
}

/// See the file comment.
#[macro_export]
macro_rules! vfs_ops {
    (
        $(#[$tm:meta])*
        $vis:vis trait $Trait:ident $(: $sup:ident $(+ $sups:ident)*)? for $Ops:ty,
            $VTable:ident<$P:ident: $Bound:path> = $Impl:ty $(, |$o:ident| $init:block)?;
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

        $vis struct $VTable<$P>(::core::marker::PhantomData<$P>);

        impl<$P: $Bound> $VTable<$P> {
            $vis const OPS: $Ops = {
                // SAFETY: zero is NULL, "not implemented"
                let mut ops: $Ops = unsafe { ::core::mem::zeroed() };
                $( $(#[cfg($cfg)])* if <$Impl>::[<HAS_ $name:upper>] { ops.$name = Some(Self::$name); } )*
                $($( $(#[cfg($ucfg)])* if <$Impl>::[<HAS_ $uname:upper>] { ops.$uname = Some(Self::$uname); } )*)?
                $( let ops = { let mut $o = ops; $init $o }; )?
                ops
            };

            $(
                $crate::vfs_ops!(@adapter_fn [$(#[cfg($cfg)])*] [$P $Impl] [$($sa)?]
                                 $name [$($arg: $ty),*] [$($ret)?]
                                 [$( ($($xarg: $xty),*) [$($xret)?] { $($xbody)* } )?]);
            )*
            $($(
                $crate::vfs_ops!(@adapter_fn [$(#[cfg($ucfg)])*] [$P $Impl] [$($usa)?]
                                 $uname [$($uarg: $uty),*] [$($uret)?] []);
            )*)?
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
    (@adapter_fn [$($cfg:tt)*] $pi:tt $sa:tt $name:ident $args:tt $ret:tt
                 [($($xarg:ident: $xty:ty),*) [$($xret:ty)?] { $($xbody:tt)* }]) => {
        $($cfg)*
        unsafe extern "C" fn $name($($xarg: $xty),*) $(-> $xret)? { $($xbody)* }
    };
    (@adapter_fn $cfg:tt $pi:tt $sa:tt $name:ident $args:tt [] []) => {
        $crate::vfs_ops!(@adapter_fn $cfg $pi $sa $name $args [()] []);
    };
    (@adapter_fn [$($cfg:tt)*] [$P:ident $Impl:ty] [$($sa:ident)?] $name:ident
                 [$($arg:ident: $ty:ty),*] [$ret:ty] []) => {
        $($cfg)*
        unsafe extern "C" fn $name<RawRet>($($arg: <$ty as $crate::util::ops::FromRaw>::Raw),*) -> RawRet
        where $ret: $crate::util::ops::IntoRaw<RawRet>
        {
            $crate::vfs_ops!(@call [$P $Impl] [$($sa)?] $name [$($arg: $ty),*])
        }
    };
    (@call [$P:ident $Impl:ty] [$sa:ident] $name:ident [$($arg:ident: $ty:ty),*]) => {
        // SAFETY: FromRaw's and SelfOf's
        unsafe {
            let this: &$Impl = $crate::util::ops::SelfOf::<$P>::self_of(&$sa);
            $(let $arg: $ty = $crate::util::ops::FromRaw::from_raw($arg);)*
            $crate::util::ops::IntoRaw::into_raw(this.$name($($arg),*))
        }
    };
    (@call [$P:ident $Impl:ty] [] $name:ident [$($arg:ident: $ty:ty),*]) => {
        // SAFETY: FromRaw's
        unsafe {
            $(let $arg: $ty = $crate::util::ops::FromRaw::from_raw($arg);)*
            $crate::util::ops::IntoRaw::into_raw(<$Impl>::$name($($arg),*))
        }
    };
}
