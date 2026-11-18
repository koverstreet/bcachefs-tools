// SPDX-License-Identifier: GPL-2.0

//! A unicode map: the tables names are normalized and casefolded with.

#[cfg(CONFIG_UNICODE)]
mod map {
    use crate::c;
    use crate::errcode::{errptr_to_result, ret_to_result_void, BchError};
    use crate::util::os_str::{qstr, OsStr};
    use core::ptr::NonNull;

    pub struct UnicodeMap(NonNull<c::unicode_map>);

    // read-only once loaded
    unsafe impl Send for UnicodeMap {}
    unsafe impl Sync for UnicodeMap {}

    impl UnicodeMap {
        /// utf8_load(): unicode @version's map, the version packed as
        /// major << 16 | minor << 8 | revision. None if it can't be loaded.
        pub fn load(version: u32) -> Option<Self> {
            // userspace's is NULL on failure, the kernel's an ERR_PTR()
            let p = NonNull::new(unsafe { c::utf8_load(version) })?;
            errptr_to_result(p).ok().map(Self)
        }

        pub fn as_ptr(&self) -> *mut c::unicode_map {
            self.0.as_ptr()
        }

        /// @name casefolded, into @out: how long it is - utf8_casefold().
        pub fn casefold(&self, name: &OsStr, out: &mut [u8]) -> Result<usize, BchError> {
            let ret = unsafe { c::utf8_casefold(self.0.as_ptr(), &qstr(name), out.as_mut_ptr(), out.len()) };
            ret_to_result_void(ret.min(0))?;
            Ok(ret as usize)
        }
    }

    impl Drop for UnicodeMap {
        fn drop(&mut self) {
            unsafe { c::utf8_unload(self.0.as_ptr()) }
        }
    }
}

#[cfg(CONFIG_UNICODE)]
pub use map::UnicodeMap;

/// Without CONFIG_UNICODE there's no map to load, so there are none.
#[cfg(not(CONFIG_UNICODE))]
pub enum UnicodeMap {}
