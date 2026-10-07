// SPDX-License-Identifier: GPL-2.0

//! Variable length integers (util/varint.c), as packed inode fields are
//! stored: the first byte's trailing ones, plus one, are the length, and
//! nine bytes hold any u64.
//!
//! These are the bounds-checked C functions, not the _fast ones: those read
//! and write up to 8 bytes past the integer, which a slice can't promise is
//! there. Both give the same values, and the same errors.

use crate::c;
use crate::errcode::{ret_to_result, BchError};

/// The most bytes an encoded integer takes.
pub const MAX_BYTES: usize = 9;

/// How many bytes @v encodes to.
pub fn encoded_len(v: u64) -> usize {
    let bits = 64 - (v | 1).leading_zeros() as usize;
    bits.div_ceil(7).min(MAX_BYTES)
}

/// Encode @v at the start of @out: how many bytes it took, as
/// bch2_varint_encode(). Panics if @out is too short - MAX_BYTES is always
/// enough.
pub fn encode(out: &mut [u8], v: u64) -> usize {
    let len = encoded_len(v);
    assert!(out.len() >= len, "varint of {len} bytes into {}", out.len());

    // Writes exactly the encoded length:
    let ret = unsafe { c::bch2_varint_encode(out.as_mut_ptr(), v) } as usize;
    debug_assert_eq!(ret, len);
    ret
}

/// Decode the integer at the start of @in: its value, and how many bytes it
/// took - varint_decode_error if it runs past the end of @in. As
/// bch2_varint_decode().
pub fn decode(in_: &[u8]) -> Result<(u64, usize), BchError> {
    let mut v = 0;
    // Reads only within @in:
    let ret = unsafe {
        c::bch2_varint_decode(in_.as_ptr(), in_.as_ptr().add(in_.len()), &mut v)
    };
    Ok((v, ret_to_result(ret)? as usize))
}
