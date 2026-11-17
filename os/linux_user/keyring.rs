// SPDX-License-Identifier: GPL-2.0

//! User keys: the linux-keyutils crate's.

use crate::c;
use crate::errcode::BchError;
use core::ffi::CStr;

pub use linux_keyutils::{Key, KeyRing, KeyRingIdentifier};

/// The user key @desc, found in the session keyring, the user's, or the user
/// session's - not read.
pub fn request_key(desc: &CStr) -> Result<Key, BchError> {
    let desc = desc.to_str().expect("a key description, \"bcachefs:<uuid>\"");
    [KeyRingIdentifier::Session, KeyRingIdentifier::User, KeyRingIdentifier::UserSession].into_iter()
        .find_map(|ring| KeyRing::from_special_id(ring, false).and_then(|r| r.search(desc)).ok())
        .ok_or(BchError::from(c::ENOKEY))
}
