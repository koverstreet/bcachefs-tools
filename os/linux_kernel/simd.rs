// SPDX-License-Identifier: GPL-2.0

//! The CPU's vector registers, for g2p's kernels - a stripe's parity: which
//! of them it has. g2p finds out itself with std; in the kernel, it's told.

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
