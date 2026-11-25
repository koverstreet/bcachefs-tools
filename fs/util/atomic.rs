// SPDX-License-Identifier: GPL-2.0

//! try_update() for the core atomics, as rustc 1.95 names fetch_update(),
//! which it deprecates: our floor, rustc 1.89, has only fetch_update(). A
//! function, not a method - an extension trait's method loses to the
//! inherent one where it exists, and its imports warn unused there. Goes
//! when the floor has try_update(): each try_update(&a, ..) is
//! a.try_update(..).

use core::sync::atomic::{self, Ordering};

pub trait Atomic {
    type V;

    fn try_update(&self, set: Ordering, fetch: Ordering, f: impl FnMut(Self::V) -> Option<Self::V>)
        -> Result<Self::V, Self::V>;
}

macro_rules! atomic {
    ($($a:ident: $v:ty),*) => {$(
        impl Atomic for atomic::$a {
            type V = $v;

            #[allow(deprecated)]
            fn try_update(&self, set: Ordering, fetch: Ordering, f: impl FnMut($v) -> Option<$v>)
                -> Result<$v, $v>
            {
                self.fetch_update(set, fetch, f)
            }
        }
    )*};
}
atomic!(AtomicU8: u8, AtomicU16: u16, AtomicU32: u32, AtomicU64: u64, AtomicUsize: usize,
        AtomicI8: i8, AtomicI16: i16, AtomicI32: i32, AtomicI64: i64, AtomicIsize: isize);

/// @a's value replaced with what @f makes of it, unless @f says None:
/// Ok(the old value), or Err(it) if @f declined.
pub fn try_update<A: Atomic>(a: &A, set: Ordering, fetch: Ordering, f: impl FnMut(A::V) -> Option<A::V>)
    -> Result<A::V, A::V>
{
    a.try_update(set, fetch, f)
}
