// SPDX-License-Identifier: GPL-2.0

//! The CPU's vector registers, for g2p's kernels - a stripe's parity: which
//! of them it has - g2p finds out itself with std; in the kernel, it's told -
//! and using them, which the kernel's code doesn't, but between
//! kernel_fpu_begin() and kernel_fpu_end().

use crate::c;
use g2p::bulk::{set_isa, Isa};

/// Whether the CPU has target feature @f, by the name g2p's kernels give it
#[cfg(target_arch = "x86_64")]
fn has(f: &str) -> bool {
    let bit = match f {
        "ssse3" => c::X86_FEATURE_SSSE3,
        "avx2" => c::X86_FEATURE_AVX2,
        "avx512f" => c::X86_FEATURE_AVX512F,
        "avx512bw" => c::X86_FEATURE_AVX512BW,
        "gfni" => c::X86_FEATURE_GFNI,
        _ => return false,
    };
    unsafe { c::rust_boot_cpu_has(bit) }
}

#[cfg(target_arch = "aarch64")]
fn has(f: &str) -> bool {
    f == "neon" && unsafe { c::system_supports_fpsimd() }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn has(_: &str) -> bool {
    false
}

/// g2p's kernels: the best the CPU has every feature of.
pub fn init() {
    let isa = Isa::ALL.iter().copied()
        .find(|i| i.features().iter().all(|f| has(f)))
        .unwrap_or(Isa::Scalar);
    unsafe { set_isa(isa) }
}

/// @f, which may use the vector registers.
#[cfg(target_arch = "x86_64")]
pub fn with<T>(f: impl FnOnce() -> T) -> T {
    unsafe { c::kernel_fpu_begin() };
    let ret = f();
    unsafe { c::kernel_fpu_end() };
    ret
}

/// @f, which may use the vector registers - from 6.19, whose state while @f's
/// preempted is saved here.
#[cfg(target_arch = "aarch64")]
pub fn with<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(linux_ge = "6.19")]
    {
        let mut state = core::mem::MaybeUninit::<c::user_fpsimd_state>::uninit();
        unsafe { c::kernel_neon_begin(state.as_mut_ptr()) };
        let ret = f();
        unsafe { c::kernel_neon_end(state.as_mut_ptr()) };
        ret
    }
    #[cfg(not(linux_ge = "6.19"))]
    {
        unsafe { c::kernel_neon_begin() };
        let ret = f();
        unsafe { c::kernel_neon_end() };
        ret
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn with<T>(f: impl FnOnce() -> T) -> T {
    f()
}
