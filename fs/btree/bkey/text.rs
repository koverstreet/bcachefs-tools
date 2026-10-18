//! Keys and positions to and from text: Debug and FromStr for positions,
//! ToText for a key's header, and BkeySCToText for a key with its value.

use super::{AllTypes, BkeySC, BkeyVal, FamilyVal};
use crate::c;
use crate::c::bpos as Bpos;
use crate::fs::Fs;
use crate::util::{Printbuf, ToText};
use core::fmt;
#[cfg(feature = "std")]
use core::str::FromStr;
#[cfg(feature = "std")]
use crate::errcode::{bch_errcode, BchError};
#[cfg(feature = "std")]
use super::{POS_MAX, POS_MIN, SPOS_MAX};

/// As Display.
impl fmt::Debug for Bpos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A bpos field that holds a u64 (inode, offset). "U64_MAX" and "U32_MAX" are
/// accepted, as positions are printed.
#[cfg(feature = "std")]
fn parse_bpos_u64(s: &str) -> Result<u64, BchError> {
    match s {
        "U64_MAX" => Ok(u64::MAX),
        "U32_MAX" => Ok(u32::MAX as u64),
        _        => s.parse().map_err(|_| BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_bpos)),
    }
}

/// Same for the snapshot field (u32).
#[cfg(feature = "std")]
fn parse_bpos_u32(s: &str) -> Result<u32, BchError> {
    match s {
        "U32_MAX" => Ok(u32::MAX),
        _        => s.parse().map_err(|_| BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_bpos)),
    }
}

#[cfg(feature = "std")]
impl FromStr for c::bpos {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "POS_MIN" {
            return Ok(POS_MIN);
        }

        if s == "POS_MAX" {
            return Ok(POS_MAX);
        }

        if s == "SPOS_MAX" {
            return Ok(SPOS_MAX);
        }

        let err = || BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_bpos);

        let mut fields = s.split(':');
        let ino_str = fields.next().ok_or_else(err)?;
        let off_str = fields.next().ok_or_else(err)?;
        let snp_str = fields.next();

        let ino: u64 = parse_bpos_u64(ino_str)?;
        let off: u64 = parse_bpos_u64(off_str)?;
        let snp: u32 = snp_str
            .map(parse_bpos_u32)
            .transpose()?
            .unwrap_or(0);

        Ok(c::bpos {
            inode:    ino,
            offset:   off,
            snapshot: snp,
        })
    }
}

pub struct BkeySCToText<'a, 'f, V: ?Sized + BkeyVal = FamilyVal<AllTypes>> {
    pub(crate) k:  BkeySC<'a, V>,
    pub(crate) fs: &'f Fs,
}

impl<V: ?Sized + BkeyVal> ToText for BkeySCToText<'_, '_, V> {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_bkey_val_to_text(out.as_raw(), self.fs.as_ptr(), self.k.into()) }
    }
}

crate::display_via_to_text!(impl[V: ?Sized + BkeyVal] BkeySCToText<'_, '_, V>);

/// Without its value.
impl ToText for c::bkey {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_bkey_to_text(out.as_raw(), self) }
    }
}
crate::display_via_to_text!(c::bkey);
