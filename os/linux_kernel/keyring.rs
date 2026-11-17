// SPDX-License-Identifier: GPL-2.0

//! User keys: the kernel's, called as the linux-keyutils crate's are -
//! userspace's.

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use core::convert::Infallible;
use core::ffi::CStr;
use core::ptr::NonNull;

/// A ref on a user key, put on drop.
pub struct Key(NonNull<c::key>);

impl Key {
    /// Its payload, as much as @buf has room for: how long the payload is, as
    /// KEYCTL_READ's.
    pub fn read<T: AsMut<[u8]>>(&self, buf: &mut T) -> Result<usize, Infallible> {
        let (buf, key) = (buf.as_mut(), self.0.as_ptr());
        // SAFETY: our ref keeps it; its payload is read under its sem
        unsafe {
            c::down_read(&raw mut (*key).sem);
            let mut len = 0;
            let data = c::rust_user_key_data(key, &mut len);
            let len = usize::from(len);
            core::ptr::copy_nonoverlapping(data.cast::<u8>(), buf.as_mut_ptr(), len.min(buf.len()));
            c::up_read(&raw mut (*key).sem);
            Ok(len)
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: our ref
        unsafe { c::key_put(self.0.as_ptr()) }
    }
}

/// The user key @desc, from the keyrings the kernel searches for this task.
pub fn request_key(desc: &CStr) -> Result<Key, BchError> {
    // SAFETY: a string
    let key = unsafe { c::request_key(&raw mut c::key_type_user, desc.as_ptr().cast(), core::ptr::null()) };
    let key = NonNull::new(key).expect("request_key(): a key, or an ERR_PTR()");
    Ok(Key(errptr_to_result(key)?))
}
