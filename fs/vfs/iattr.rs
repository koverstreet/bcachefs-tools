// SPDX-License-Identifier: GPL-2.0

//! The VFS's struct iattr, for Rust: what a setattr asks to change, each
//! field Some where ia_valid has its ATTR_* bit.

use crate::c;

use super::inode::{ATTR_ATIME, ATTR_CTIME, ATTR_GID, ATTR_MODE, ATTR_MTIME, ATTR_SIZE, ATTR_UID};

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
        size:  i64           = ATTR_SIZE,  ia_size;
        atime: c::timespec64 = ATTR_ATIME, ia_atime;
        mtime: c::timespec64 = ATTR_MTIME, ia_mtime;
        ctime: c::timespec64 = ATTR_CTIME, ia_ctime;
    }
}
