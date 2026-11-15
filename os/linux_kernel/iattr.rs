// SPDX-License-Identifier: GPL-2.0

//! The VFS's struct iattr, for Rust: what a setattr asks to change, each
//! field Some where ia_valid has its ATTR_* bit.

use crate::c;

/// <linux/fs.h>'s ATTR_*: an iattr's ia_valid, and update_after_write()'s
/// @fields.
pub const ATTR_MODE:  u32 = 1 << 0;
pub const ATTR_UID:   u32 = 1 << 1;
pub const ATTR_GID:   u32 = 1 << 2;
pub const ATTR_SIZE:  u32 = 1 << 3;
pub const ATTR_ATIME: u32 = 1 << 4;
pub const ATTR_MTIME: u32 = 1 << 5;
pub const ATTR_CTIME: u32 = 1 << 6;

#[repr(transparent)]
pub struct Iattr(c::iattr);

/// The fields that are plain values: name, type = its ATTR_*, the field.
macro_rules! fields {
    ($($name:ident: $t:ty = $attr:ident, $field:ident;)*) => {$(
        pub fn $name(&self) -> Option<$t> {
            self.has($attr).then_some(self.0.$field)
        }
    )*};
}

impl Iattr {
    pub fn of(attr: &c::iattr) -> &Self {
        // SAFETY: repr(transparent)
        unsafe { &*core::ptr::from_ref(attr).cast() }
    }

    /// ia_valid: the ATTR_* to change.
    pub fn valid(&self) -> u32 {
        self.0.ia_valid
    }

    fn has(&self, attr: u32) -> bool {
        self.valid() & attr != 0
    }

    pub fn size(&self) -> Option<u64> {
        self.has(ATTR_SIZE).then_some(self.0.ia_size as u64)
    }

    pub fn vfsuid(&self) -> Option<c::vfsuid_t> {
        // SAFETY: either arm of the union is an id, any value of which is one
        self.has(ATTR_UID).then(|| unsafe { *self.0.__bindgen_anon_1.ia_vfsuid.as_ref() })
    }

    pub fn vfsgid(&self) -> Option<c::vfsgid_t> {
        // SAFETY: as vfsuid()
        self.has(ATTR_GID).then(|| unsafe { *self.0.__bindgen_anon_2.ia_vfsgid.as_ref() })
    }

    fields! {
        mode:  c::umode_t    = ATTR_MODE,  ia_mode;
        atime: c::timespec64 = ATTR_ATIME, ia_atime;
        mtime: c::timespec64 = ATTR_MTIME, ia_mtime;
        ctime: c::timespec64 = ATTR_CTIME, ia_ctime;
    }
}
