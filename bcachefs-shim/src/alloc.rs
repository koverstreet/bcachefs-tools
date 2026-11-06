// SPDX-License-Identifier: GPL-2.0
//! Userspace stand-in for the subset of `kernel::alloc` that fs/ uses, with
//! the same names as the kernel's, so fs/ code is written once against them.
//! Flags, where they're taken, are ignored - userspace has one way to
//! allocate.

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct AllocError;

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Flags(u32);

impl core::ops::BitOr for Flags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Flags(self.0 | rhs.0)
    }
}

pub mod flags {
    use super::Flags;

    pub const GFP_KERNEL: Flags = Flags(0);
    pub const GFP_NOWAIT: Flags = Flags(1);
    pub const GFP_ATOMIC: Flags = Flags(1);
    pub const __GFP_NOWARN: Flags = Flags(2);
}

unsafe extern "C" {
    fn realloc(p: *mut core::ffi::c_void, size: usize) -> *mut core::ffi::c_void;
    fn run_shrinkers(gfp_mask: crate::c::gfp_t, allocation_failed: bool);
}

/// krealloc(), as the shim's slab.h has it: tried ten times, the shrinkers run
/// between - if @gfp lets them - its memory what C's kfree() - free() - takes
/// back. Null if it can't.
///
/// # Safety
/// @p is null, or malloc()'s and not used after unless this fails.
pub unsafe fn krealloc(p: *mut u8, size: usize, gfp: crate::c::gfp_t) -> *mut u8 {
    for _ in 0..10 {
        // not 0: realloc() would free @p
        let new = unsafe { realloc(p.cast(), size.max(1)) };
        if !new.is_null() {
            return new.cast();
        }

        unsafe { run_shrinkers(gfp, true) };
    }
    core::ptr::null_mut()
}

