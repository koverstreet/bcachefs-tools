// SPDX-License-Identifier: GPL-2.0

//! Pages mapped as one buffer: vmap().

use crate::c;
use crate::os::kernel::PAGE_SIZE;
use crate::util::volatile::VolatileSlice;
use core::ptr::NonNull;

/// Unmapped on drop.
pub struct Vmap {
    p:     NonNull<u8>,
    pages: usize,
}

impl Vmap {
    /// None if there's no room for it.
    ///
    /// # Safety
    /// @pages are pages, which outlive it, and whose memory only C and
    /// VolatileSlices touch.
    pub unsafe fn new(pages: &mut [*mut c::page]) -> Option<Self> {
        let p = unsafe { c::rust_vmap(pages.as_mut_ptr(), pages.len() as u32) };
        NonNull::new(p.cast()).map(|p| Vmap { p, pages: pages.len() })
    }

    pub fn data(&self) -> VolatileSlice<'_> {
        unsafe { VolatileSlice::from_raw(self.p, self.pages * PAGE_SIZE) }
    }
}

impl Drop for Vmap {
    fn drop(&mut self) {
        unsafe { c::vunmap(self.p.as_ptr().cast()) }
    }
}
