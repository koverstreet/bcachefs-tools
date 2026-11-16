// SPDX-License-Identifier: GPL-2.0

//! A dir_context: readdir's output - getdents', or nfsd's - and the position
//! it's up to.

use core::ffi::c_int;
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::c;
use crate::os::ops::from_raw;

/// A dir_context, held as the pointer it came as: its actor may take it to
/// the struct it heads - container_of(), as getdents' does - and a
/// &mut dir_context reaches only the dir_context.
pub struct DirContext<'c>(NonNull<c::dir_context>, PhantomData<&'c mut c::dir_context>);

from_raw! {
    /// iterate_shared()'s.
    DirContext<'a> = *mut c::dir_context => |r| DirContext(NonNull::new(r).expect("a dir_context"), PhantomData);
}

impl DirContext<'_> {
    pub fn pos(&self) -> u64 {
        unsafe { (*self.0.as_ptr()).pos as u64 }
    }

    pub fn set_pos(&mut self, pos: u64) {
        unsafe { (*self.0.as_ptr()).pos = pos as c::loff_t };
    }

    /// @name, inode @ino of type @d_type, at @pos: false once full.
    pub fn emit(&mut self, name: &[u8], pos: u64, ino: u64, d_type: u32) -> bool {
        let ctx = self.0.as_ptr();
        unsafe {
            let actor = (*ctx).actor.expect("a dir_context's actor");
            actor(ctx, name.as_ptr().cast(), name.len() as c_int, pos as c::loff_t, ino, d_type)
        }
    }
}
