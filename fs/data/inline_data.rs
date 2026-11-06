// SPDX-License-Identifier: GPL-2.0

//! Inline data extents: a file's data in its extent key - inline_data, or
//! reflinked, indirect_inline_data - the Inline family, BKEY_TYPE_inline_data,
//! and where in each type's value the data is.

use crate::btree::bkey::{BkeyFamily, BkeyS, BkeySC, BkeyV, ByFlag, FamilyVal, FieldOffsets};
use crate::c;
use crate::prt;
use crate::util::Printbuf;
use cstruct_macros::c_export;

/// The data itself, inline in the value, direct or indirect,
/// BKEY_TYPE_inline_data: a key armed to it is a BkeySC<'_, Inline>,
/// BkeyS<'_, Inline>, &BkeyI<Inline>.
pub type Inline = FamilyVal<ByFlag<{ c::BKEY_TYPE_inline_data }>>;

pub static DATA: FieldOffsets = FieldOffsets::new(c::BKEY_TYPE_inline_data, "data");

impl c::bkey {
    pub fn extent_is_inline_data(&self) -> bool {
        <Inline as BkeyFamily>::contains(self)
    }
}

/// bkey_extent_is_inline_data(), for C.
#[c_export]
pub fn bch2_bkey_is_inline_data(k: &c::bkey) -> bool {
    k.extent_is_inline_data()
}

/// An inline data key's data[].
pub struct InlineDataRef<'a> {
    pub data: &'a [u8],
}

impl<'a> InlineDataRef<'a> {
    /// None if it isn't inline data.
    pub fn of(k: BkeySC<'a>) -> Option<Self> {
        Some(Self { data: k.val_bytes().get(DATA.of(k.k)?..)? })
    }

    /// Its length, and the first 32 bytes, in hex.
    pub fn to_text(&self, out: &mut Printbuf) {
        prt!(out, "datalen {}: ", self.data.len());
        for b in &self.data[..self.data.len().min(32)] {
            prt!(out, "{b:02x}");
        }
    }
}

/// The same, to change: a cut's, or a sanitized dump's.
pub struct InlineDataMut<'a> {
    pub data: &'a mut [u8],
}

impl<'a> InlineDataMut<'a> {
    /// None if it isn't inline data.
    pub fn of(k: BkeyS<'a>) -> Option<Self> {
        let at = DATA.of(k.k())?;
        Some(Self { data: k.into_val_bytes_mut().get_mut(at..)? })
    }
}
