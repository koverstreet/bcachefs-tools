// SPDX-License-Identifier: GPL-2.0

//! Inline data extents: a file's data in its extent key - inline_data, or
//! reflinked, indirect_inline_data - the Inline family, BKEY_TYPE_inline_data,
//! and where in each type's value the data is. From inline_data_csum (1.40)
//! their _v2 types carry a checksum of their own over data[]: a node's bset
//! checksum can't say which of its keys went bad.

use crate::bkey_fsck_err_on;
use crate::btree::bkey::{BkeyFamily, BkeyS, BkeySC, BkeyV, ByFlag, FamilyVal, FieldOffsets};
use crate::btree::bkey::BkeyOps;
use crate::c;
use crate::data::checksum::csum_err_msg;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::prt;
use crate::util::ffi::Opaque;
use crate::util::vstructs::{cast_plain, cast_plain_mut};
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_export;

/// The data itself, inline in the value, direct or indirect,
/// BKEY_TYPE_inline_data: a key armed to it is a BkeySC<'_, Inline>,
/// BkeyS<'_, Inline>, &BkeyI<Inline>.
pub type Inline = FamilyVal<ByFlag<{ c::BKEY_TYPE_inline_data }>>;

pub static DATA: FieldOffsets = FieldOffsets::new(c::BKEY_TYPE_inline_data, "data");
pub static CSUM: FieldOffsets = FieldOffsets::optional(c::BKEY_TYPE_inline_data, "csum");

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

/// An inline data key's data[], and the checksum over all of it, if it has
/// one. A key with one starts @offset sectors into data[]: a cut moves it,
/// and never touches data[], which the checksum is of as written.
pub struct InlineDataRef<'a> {
    pub csum: Option<&'a c::InlineDataCsum>,
    pub data: &'a [u8],
}

impl<'a> InlineDataRef<'a> {
    /// None if it isn't inline data.
    pub fn of(k: BkeySC<'a>) -> Option<Self> {
        let val = k.val_bytes();
        let csum = match CSUM.of(k.k) {
            Some(at) => Some(val.get(at..).and_then(cast_plain)?),
            None     => None,
        };
        Some(Self { csum, data: val.get(DATA.of(k.k)?..)? })
    }

    /// Where the key's start is in data[], in bytes.
    pub fn start(&self) -> usize {
        self.csum.map_or(0, |c| (c.offset.get() as usize) << 9)
    }

    /// Ok if it has no checksum or its data matches it.
    pub fn verify(&self) -> Result<(), CsumMismatch> {
        let Some(h) = self.csum else { return Ok(()) };
        let ty = c::bch_csum_type(h.csum_type.into());
        let got = ty.checksum_unkeyed(self.data);
        if got == h.csum { Ok(()) } else { Err(CsumMismatch { ty, want: h.csum, got }) }
    }

    /// Its checksum, if it has one, its length, and the first 32 bytes of its
    /// data, in hex.
    pub fn to_text(&self, out: &mut Printbuf) {
        if let Some(h) = self.csum {
            let ty = c::bch_csum_type(h.csum_type.into());
            prt!(out, "csum {ty} ");
            h.csum.to_text(out, ty);
            prt!(out, " offset {} ", h.offset.get());
        }
        prt!(out, "datalen {}: ", self.data.len());
        for b in &self.data[..self.data.len().min(32)] {
            prt!(out, "{b:02x}");
        }
    }
}

/// The same, to change: a cut's, or a sanitized dump's.
pub struct InlineDataMut<'a> {
    pub csum: Option<&'a mut c::InlineDataCsum>,
    pub data: &'a mut [u8],
}

impl<'a> InlineDataMut<'a> {
    /// None if it isn't inline data.
    pub fn of(k: BkeyS<'a>) -> Option<Self> {
        let (data_at, csum_at) = (DATA.of(k.k())?, CSUM.of(k.k()));
        let (head, data) = k.into_val_bytes_mut().split_at_mut_checked(data_at)?;
        let csum = match csum_at {
            Some(at) => Some(head.get_mut(at..).and_then(cast_plain_mut)?),
            None     => None,
        };
        Some(Self { csum, data })
    }
}

/// An inline data key's data that doesn't match its checksum.
pub struct CsumMismatch {
    pub ty:   c::bch_csum_type,
    pub want: c::bch_csum,
    pub got:  c::bch_csum,
}

impl CsumMismatch {
    pub fn to_text(&self, out: &mut Printbuf) {
        csum_err_msg(out, self.ty, self.want, self.got)
    }
}

impl c::InlineDataCsum {
    /// For @data, written now with @opts: offset 0, the checksum @opts'
    /// data_checksum says - none for none, or for nocow - never an encryption
    /// type, whatever the filesystem is.
    pub fn new(fs: &Fs, opts: &c::bch_inode_opts, data: &[u8]) -> Self {
        let ty = if opts.nocow != 0 {
            c::bch_csum_type::BCH_CSUM_none
        } else {
            c::bch_csum_opt(opts.data_checksum.into()).to_type(fs, true)
        };
        Self { csum_type: ty.0 as u8, csum: ty.checksum_unkeyed(data), ..Default::default() }
    }
}

pub(crate) fn csum_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let h = InlineDataRef::of(v.k).and_then(|d| d.csum).expect("an inline data key with a checksum");
    let ty = c::bch_csum_type(h.csum_type.into());
    bkey_fsck_err_on!(v, ty.0 >= c::bch_csum_type::BCH_CSUM_NR.0 || ty.is_encryption(),
                      id::inline_data_csum_type_bad, "inline data with checksum type {}", h.csum_type)
}

impl BkeyOps for c::InlineData {
    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        InlineDataRef::of(k).expect("inline data").to_text(out)
    }
}

impl BkeyOps for c::InlineDataV2 {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        csum_validate(v)
    }

    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        InlineDataRef::of(k).expect("inline data").to_text(out)
    }
}

/// For a read of @k @skip bytes past its start: its data from there, @len
/// bytes of it - none past data[], which reads as zeroes.
#[c_export]
pub fn bch2_inline_data_from(k: BkeySC<'_>, skip: c_uint, len: &mut c_uint) -> *const u8 {
    let d = InlineDataRef::of(k).expect("inline data");
    let data = d.data.get(d.start() + skip as usize..).unwrap_or_default();
    *len = data.len() as c_uint;
    data.as_ptr()
}

/// For a read of @k: 0, or data_read_csum_err with the mismatch in @out.
#[c_export]
pub fn bch2_inline_data_verify(c: &Opaque<Fs>, k: BkeySC<'_>, out: &mut Printbuf) -> c_int {
    let Err(e) = InlineDataRef::of(k).expect("inline data").verify() else { return 0 };
    e.to_text(out);
    ret_to_c(Fs::from_c(c).throw::<()>(c::bch_errcode::BCH_ERR_data_read_csum_err))
}
