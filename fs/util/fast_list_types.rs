// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{GenRadix, c_opaque, c_default};
use crate::util::rcu::ReadGuard;
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};
use cstruct_macros::CStruct;

c_opaque!(fast_list_pcpu);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct fast_list {
    pub items: GenRadix<*mut c_void>,
    pub slots_allocated: c::ida,
    #[c("struct fast_list_pcpu __percpu *buffer")]
    pub buffer: *mut c::fast_list_pcpu,
}
c_default!(fast_list);

impl fast_list {
    /// fast_list_for_each_from(), with each item's slot. Other CPUs change the
    /// slots as it's read: each is read once, atomically.
    ///
    /// XXX: an item is only safe to use if it's freed a grace period after
    /// removal, and nothing here says so. Once the owners are Rust: fast_list<T>,
    /// items &'g T, with an unsafe marker on T the owner asserts that with.
    /// async_objs' items don't hold to it today: see debug/async_objs.rs.
    pub fn items_from<'g>(&'g self, idx: usize, _rcu: &'g ReadGuard)
        -> impl Iterator<Item = (usize, NonNull<c_void>)> + 'g
    {
        self.items.iter_from_raw(idx).filter_map(|(i, slot)| {
            // SAFETY: a slot of the radix, a pointer, which C writes as one
            let item = unsafe { AtomicPtr::from_ptr(slot.as_ptr()) }.load(Ordering::Acquire);
            NonNull::new(item).map(|item| (i, item))
        })
    }
}
