use crate::c;
#[cfg(feature = "std")]
use crate::errcode::{bch_errcode, BchError};
#[cfg(feature = "std")]
use std::{fmt, ffi::CString, ffi::CStr, str::FromStr};

impl c::btree_id {
    /// Convert from raw u32. Returns None for unknown built-in btree IDs.
    pub fn from_raw(id: u32) -> Option<Self> {
        crate::BTREE_IDS_KNOWN.get(id as usize).copied()
    }

    /// Iterate over all known btree IDs.
    pub fn iter_known() -> impl Iterator<Item = Self> {
        crate::BTREE_IDS_KNOWN.iter().copied()
    }
}

impl From<c::btree_id> for u32 {
    fn from(id: c::btree_id) -> u32 {
        id as u32
    }
}

/// Get a btree ID name string.
#[cfg(feature = "std")]
pub fn btree_id_str(id: u32) -> String {
    match c::btree_id::from_raw(id) {
        Some(btree_id) => format!("{}", btree_id),
        None => format!("(unknown btree {})", id),
    }
}

#[cfg(feature = "std")]
impl fmt::Display for c::btree_id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = unsafe { CStr::from_ptr(c::bch2_btree_id_str(*self)) };
        f.write_str(&s.to_string_lossy())
    }
}

#[cfg(feature = "std")]
impl FromStr for c::btree_id {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = CString::new(s)
            .map_err(|_| BchError::from_errcode(bch_errcode::BCH_ERR_EINVAL_parse_btree_id))?;
        let p = s.as_ptr();

        let v =
            unsafe { c::match_string(c::__bch2_btree_ids[..].as_ptr(), (-1_isize) as usize, p) };
        c::btree_id::from_raw(v as u32)
            .ok_or(BchError::from_errcode(bch_errcode::BCH_ERR_EINVAL_parse_btree_id))
    }
}

#[cfg(feature = "std")]
impl FromStr for c::bch_bkey_type {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = CString::new(s)
            .map_err(|_| BchError::from_errcode(bch_errcode::BCH_ERR_EINVAL_parse_bkey_type))?;
        let p = s.as_ptr();

        let v = unsafe { c::match_string(c::bch2_bkey_types[..].as_ptr(), (-1_isize) as usize, p) };
        if v >= 0 {
            Ok(c::bch_bkey_type(v as u32))
        } else {
            Err(BchError::from_errcode(bch_errcode::BCH_ERR_EINVAL_parse_bkey_type))
        }
    }
}
