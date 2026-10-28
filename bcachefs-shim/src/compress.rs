// SPDX-License-Identifier: GPL-2.0
//! The kernel's lz4 and zlib APIs, over liblz4 and libz. The kernel's work in
//! a workspace the caller allocates; the libraries allocate their own, and
//! take none, under the same names - so include/linux's are macros, which
//! bindgen can't see. zstd's kernel API is linux/zstd_*_module.c's, in C.

#![allow(non_snake_case)]

use crate::c::z_stream;
use core::ffi::{c_char, c_int, c_void};

mod lib {
    use super::*;

    extern "C" {
        pub fn LZ4_compress_destSize(src: *const c_char, dst: *mut c_char, src_size: *mut c_int,
                                     dst_capacity: c_int) -> c_int;
        pub fn LZ4_compress_HC(src: *const c_char, dst: *mut c_char, src_size: c_int,
                               dst_capacity: c_int, level: c_int) -> c_int;
        pub fn deflateInit2_(strm: *mut z_stream, level: c_int, method: c_int, window_bits: c_int,
                             mem_level: c_int, strategy: c_int, version: *const c_char,
                             stream_size: c_int) -> c_int;
        pub fn deflate(strm: *mut z_stream, flush: c_int) -> c_int;
        pub fn deflateEnd(strm: *mut z_stream) -> c_int;
        pub fn inflateInit2_(strm: *mut z_stream, window_bits: c_int, version: *const c_char,
                             stream_size: c_int) -> c_int;
        pub fn inflate(strm: *mut z_stream, flush: c_int) -> c_int;
        pub fn inflateEnd(strm: *mut z_stream) -> c_int;
    }
}

const VERSION: *const c_char = crate::c::ZLIB_VERSION.as_ptr().cast();
const STREAM_SIZE: c_int = core::mem::size_of::<z_stream>() as c_int;

/// # Safety
/// As liblz4's.
pub unsafe fn LZ4_compress_destSize(src: *const c_char, dst: *mut c_char, src_size: *mut c_int,
                                    dst_capacity: c_int, _wrkmem: *mut c_void) -> c_int {
    unsafe { lib::LZ4_compress_destSize(src, dst, src_size, dst_capacity) }
}

/// # Safety
/// As liblz4's.
pub unsafe fn LZ4_compress_HC(src: *const c_char, dst: *mut c_char, src_size: c_int,
                              dst_capacity: c_int, level: c_int, _wrkmem: *mut c_void) -> c_int {
    unsafe { lib::LZ4_compress_HC(src, dst, src_size, dst_capacity, level) }
}

pub fn zlib_deflate_workspacesize(_window_bits: c_int, _mem_level: c_int) -> c_int {
    0
}

pub fn zlib_inflate_workspacesize() -> c_int {
    0
}

/// # Safety
/// As libz's, for this and the rest.
pub unsafe fn zlib_deflateInit2(strm: *mut z_stream, level: c_int, method: c_int, window_bits: c_int,
                                mem_level: c_int, strategy: c_int) -> c_int {
    unsafe { lib::deflateInit2_(strm, level, method, window_bits, mem_level, strategy, VERSION, STREAM_SIZE) }
}

pub unsafe fn zlib_deflate(strm: *mut z_stream, flush: c_int) -> c_int {
    unsafe { lib::deflate(strm, flush) }
}

pub unsafe fn zlib_deflateEnd(strm: *mut z_stream) -> c_int {
    unsafe { lib::deflateEnd(strm) }
}

pub unsafe fn zlib_inflateInit2(strm: *mut z_stream, window_bits: c_int) -> c_int {
    unsafe { lib::inflateInit2_(strm, window_bits, VERSION, STREAM_SIZE) }
}

pub unsafe fn zlib_inflate(strm: *mut z_stream, flush: c_int) -> c_int {
    unsafe { lib::inflate(strm, flush) }
}

pub unsafe fn zlib_inflateEnd(strm: *mut z_stream) -> c_int {
    unsafe { lib::inflateEnd(strm) }
}
