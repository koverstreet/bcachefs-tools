// SPDX-License-Identifier: GPL-2.0

//! Variable length integers, as packed inode fields are stored: the first
//! byte's trailing ones, plus one, are the length. Up to 8 bytes hold the
//! value shifted up past those; a first byte of all ones is followed by the
//! u64, little endian - so nine bytes hold any u64.
//!
//! encode() writes only the integer's bytes, and decode() reads only within
//! its slice - a whole u64 at once where the slice has 8 bytes, as most do.
//! The _fast ones move a whole u64 whatever the length, so the buffer needs
//! room past where its data ends - as C's did, up to 8 bytes: what the slack
//! after a btree node's keys, a cached key, or bkey_reassemble_room()'s copy
//! is for. Both give the same values, and the same errors.
//!
//! What's under 8 bytes is loaded and stored by overlapping power of two
//! accesses - what an inlined memcpy() of a length known only at runtime is
//! in C - never a slice copy, which is a call to memcpy().

use crate::c::bch_errcode;
use crate::errcode::BchError;

/// The most bytes an encoded integer takes.
pub const MAX_BYTES: usize = 9;

/// How many bytes @v encodes to: its bits, sevens, rounded up.
#[inline]
pub fn encoded_len(v: u64) -> usize {
    ((70 - (v | 1).leading_zeros()) / 7).min(MAX_BYTES as u32) as usize
}

/// An encoding shorter than MAX_BYTES, @len bytes of it: @v shifted up past
/// @len - 1 ones and a zero.
#[inline]
fn short(v: u64, len: usize) -> u64 {
    (v << len) | ((1 << (len - 1)) - 1)
}

/// The value of an encoding shorter than MAX_BYTES, from a u64 of its first
/// 8 bytes - what's past @len masked off.
#[inline]
fn value(w: u64, len: usize) -> u64 {
    (w >> len) & !(!0 << (7 * len))
}

/// @b, at most 8 bytes, little endian.
#[inline]
fn load(b: &[u8]) -> u64 {
    let n = b.len();
    if let (Some(lo), Some(hi)) = (b.first_chunk::<4>(), b.last_chunk::<4>()) {
        u32::from_le_bytes(*lo) as u64 | (u32::from_le_bytes(*hi) as u64) << (8 * (n - 4))
    } else if let (Some(lo), Some(hi)) = (b.first_chunk::<2>(), b.last_chunk::<2>()) {
        u16::from_le_bytes(*lo) as u64 | (u16::from_le_bytes(*hi) as u64) << (8 * (n - 2))
    } else {
        b.first().map_or(0, |&x| x as u64)
    }
}

/// The low bytes of @w into @out, at most 8 of them.
#[inline]
fn store(out: &mut [u8], w: u64) {
    let n = out.len();
    if n >= 4 {
        let hi = (w >> (8 * (n - 4))) as u32;
        *out.first_chunk_mut().unwrap() = (w as u32).to_le_bytes();
        *out.last_chunk_mut().unwrap() = hi.to_le_bytes();
    } else if n >= 2 {
        let hi = (w >> (8 * (n - 2))) as u16;
        *out.first_chunk_mut().unwrap() = (w as u16).to_le_bytes();
        *out.last_chunk_mut().unwrap() = hi.to_le_bytes();
    } else if let Some(x) = out.first_mut() {
        *x = w as u8;
    }
}

fn decode_error() -> BchError {
    BchError::from(bch_errcode::BCH_ERR_varint_decode_error)
}

/// Encode @v at the start of @out: how many bytes it took. Panics if @out is
/// too short - MAX_BYTES is always enough.
#[inline]
pub fn encode(out: &mut [u8], v: u64) -> usize {
    let len = encoded_len(v);
    if len < MAX_BYTES {
        store(&mut out[..len], short(v, len));
    } else {
        let out: &mut [u8; MAX_BYTES] = out.first_chunk_mut().expect("varint::encode(): no 9 bytes to write");
        out[0] = u8::MAX;
        *out[1..].first_chunk_mut().unwrap() = v.to_le_bytes();
    }
    len
}

/// encode(), writing 8 bytes at the start of @out whatever the length - 9
/// for MAX_BYTES. Panics if @out is too short for that.
#[inline]
pub fn encode_fast(out: &mut [u8], v: u64) -> usize {
    let len = encoded_len(v);
    if len < MAX_BYTES {
        *out.first_chunk_mut().expect("varint::encode_fast(): no 8 bytes to write") = short(v, len).to_le_bytes();
    } else {
        let out: &mut [u8; MAX_BYTES] = out.first_chunk_mut().expect("varint::encode_fast(): no 9 bytes to write");
        out[0] = u8::MAX;
        *out[1..].first_chunk_mut().unwrap() = v.to_le_bytes();
    }
    len
}

/// Decode the integer at the start of @in_: its value, and how many bytes it
/// took - varint_decode_error if it runs past the end of @in_.
#[inline]
pub fn decode(in_: &[u8]) -> Result<(u64, usize), BchError> {
    let len = in_.first().map_or(1, |b| b.trailing_ones() as usize + 1);
    let b = in_.get(..len).ok_or_else(decode_error)?;

    if let Some(b) = b.first_chunk::<MAX_BYTES>() {
        Ok((u64::from_le_bytes(*b[1..].first_chunk().unwrap()), len))
    } else if let Some(w) = in_.first_chunk() {
        Ok((value(u64::from_le_bytes(*w), len), len))
    } else {
        Ok((load(b) >> len, len))
    }
}

/// decode(), reading 8 bytes from the start of @in_ whatever the length - 9
/// for MAX_BYTES: @in_ runs into the buffer's slack, and the integer's data
/// ends at @end - varint_decode_error if the integer runs past it. Panics if
/// @in_ is too short to read.
#[inline]
pub fn decode_fast(in_: &[u8], end: usize) -> Result<(u64, usize), BchError> {
    let w = u64::from_le_bytes(*in_.first_chunk().expect("varint::decode_fast(): no 8 bytes to read"));
    let len = (w as u8).trailing_ones() as usize + 1;
    if len > end {
        return Err(decode_error());
    }

    if len < MAX_BYTES {
        Ok((value(w, len), len))
    } else {
        let b: &[u8; MAX_BYTES] = in_.first_chunk().expect("varint::decode_fast(): no 9 bytes to read");
        Ok((u64::from_le_bytes(*b[1..].first_chunk().unwrap()), len))
    }
}
