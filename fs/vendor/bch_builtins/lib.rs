// SPDX-License-Identifier: MIT OR Apache-2.0
//! The integer intrinsics rustc calls that the kernel has none of: u128
//! division and remainder, which LLVM lowers to __udivti3, __umodti3, __divti3
//! and __modti3 on every 64-bit target. Built as an ordinary crate (Makefile.rust),
//! so our Rust divides u128s like any other integer.
//!
//! specialized_div_rem/ is compiler-builtins 0.1.159's src/int/specialized_div_rem,
//! verbatim: the last release that builds on rustc 1.85 - after it, let-chains -
//! and the same algorithms as compiler-builtins' since. It's the
//! specialized-div-rem crate's own work, MIT OR Apache-2.0, not one of
//! compiler-builtins' ports of compiler-rt (LICENSE.txt). The functions here
//! are compiler-builtins' udiv.rs and sdiv.rs, without their intrinsics!
//! macro.
//!
//! Only u128's: u64 division is an instruction on every 64-bit target, and
//! 32-bit arm's (__aeabi_uldivmod) isn't here.

#![no_std]
// Or LLVM may recognize a loop here as an intrinsic, and call itself
#![no_builtins]
// specialized_div_rem's zero_div_fn
#![feature(core_intrinsics)]
#![allow(internal_features, improper_ctypes_definitions)]

mod specialized_div_rem;
use specialized_div_rem::u128_div_rem;

#[no_mangle]
pub extern "C" fn __udivti3(n: u128, d: u128) -> u128 {
    u128_div_rem(n, d).0
}

#[no_mangle]
pub extern "C" fn __umodti3(n: u128, d: u128) -> u128 {
    u128_div_rem(n, d).1
}

/// Signed from unsigned: the quotient negative if exactly one operand is, the
/// remainder taking the dividend's sign.
fn i128_div_rem(a: i128, b: i128) -> (i128, i128) {
    let (quo, rem) = u128_div_rem(a.unsigned_abs(), b.unsigned_abs());
    let (quo, rem) = (quo as i128, rem as i128);

    (if (a < 0) != (b < 0) { quo.wrapping_neg() } else { quo },
     if a < 0 { rem.wrapping_neg() } else { rem })
}

#[no_mangle]
pub extern "C" fn __divti3(a: i128, b: i128) -> i128 {
    i128_div_rem(a, b).0
}

#[no_mangle]
pub extern "C" fn __modti3(a: i128, b: i128) -> i128 {
    i128_div_rem(a, b).1
}
