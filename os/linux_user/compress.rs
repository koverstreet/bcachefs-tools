// SPDX-License-Identifier: GPL-2.0

//! The kernel's lz4, zlib and zstd APIs over liblz4, libz and libzstd. lz4 and
//! zlib allocate their own workspaces - theirs are 0 bytes, and the caller's go
//! unused; zstd's contexts are made in the caller's, as the kernel's are.

fn int(n: usize) -> core::ffi::c_int {
    n.try_into().unwrap_or_else(|_| panic!("a codec's length of {n}: past a c_int"))
}

pub mod lz4 {
    use super::int;
    use crate::c;
    use crate::util::volatile::VolatileSlice;
    use core::ffi::c_int;
    use core::mem::MaybeUninit;

    pub const MEM_COMPRESS: usize = 0;
    pub const HC_MEM_COMPRESS: usize = 0;
    /// The kernel's threshold for lz4 against lz4hc.
    pub const HC_MIN_CLEVEL: u32 = 3;

    /// As much of @src as fits in @dst, compressed: its compressed length, 0
    /// on error, and how much of @src that was.
    pub fn compress_dest_size(src: &[u8], dst: VolatileSlice<'_>, _ws: &mut [MaybeUninit<u8>]) -> (usize, usize) {
        let mut src_len = int(src.len());
        let ret = unsafe {
            c::LZ4_compress_destSize(src.as_ptr().cast(), dst.as_ptr().cast(), &mut src_len, int(dst.len()))
        };
        (ret as usize, src_len as usize)
    }

    /// @src's compressed length in @dst: 0 if it didn't fit.
    pub fn compress_hc(src: &[u8], dst: VolatileSlice<'_>, level: u32, _ws: &mut [MaybeUninit<u8>]) -> usize {
        let ret = unsafe {
            c::LZ4_compress_HC(src.as_ptr().cast(), dst.as_ptr().cast(), int(src.len()), int(dst.len()), level as c_int)
        };
        ret as usize
    }

    /// At least @target bytes of @src decompressed into @dst: how many, or
    /// None if @src is malformed.
    pub fn decompress_safe_partial(src: VolatileSlice<'_>, dst: VolatileSlice<'_>, target: usize) -> Option<usize> {
        let ret = unsafe {
            c::LZ4_decompress_safe_partial(src.as_ptr().cast_const().cast(), dst.as_ptr().cast(),
                                           int(src.len()), int(target), int(dst.len()))
        };
        ret.try_into().ok()
    }
}

pub mod zlib {
    use super::int;
    use crate::c::{self, z_stream};
    use crate::util::volatile::VolatileSlice;
    use core::ffi::c_int;
    use core::mem::MaybeUninit;

    /// zlib's default, which it keeps to itself: zutil.h's
    pub const DEF_MEM_LEVEL: c_int = 8;

    const VERSION: *const core::ffi::c_char = c::ZLIB_VERSION.as_ptr().cast();
    const STREAM_SIZE: c_int = size_of::<z_stream>() as c_int;

    pub fn deflate_workspace_size(_window_bits: c_int, _mem_level: c_int) -> usize {
        0
    }

    pub fn inflate_workspace_size() -> usize {
        0
    }

    fn stream(src: *const u8, src_len: usize, dst: VolatileSlice<'_>) -> z_stream {
        z_stream {
            next_in:   src.cast_mut().cast(),
            avail_in:  int(src_len) as _,
            next_out:  dst.as_ptr().cast(),
            avail_out: int(dst.len()) as _,
            ..Default::default()
        }
    }

    /// @src deflated into @dst, all of it: its compressed length, or None if
    /// it didn't fit.
    pub fn deflate(src: &[u8], dst: VolatileSlice<'_>, _ws: &mut [MaybeUninit<u8>], level: c_int,
                   window_bits: c_int, mem_level: c_int, strategy: c_int) -> Option<usize> {
        let mut strm = stream(src.as_ptr(), src.len(), dst);
        unsafe {
            if c::deflateInit2_(&mut strm, level, c::Z_DEFLATED as c_int, window_bits, mem_level, strategy,
                                VERSION, STREAM_SIZE) != c::Z_OK as c_int {
                return None;
            }
            let done = c::deflate(&mut strm, c::Z_FINISH as c_int) == c::Z_STREAM_END as c_int;
            (c::deflateEnd(&mut strm) == c::Z_OK as c_int && done).then_some(strm.total_out as usize)
        }
    }

    /// @src inflated into @dst: how much of @dst it left, or None if @src is
    /// malformed or didn't end.
    pub fn inflate(src: VolatileSlice<'_>, dst: VolatileSlice<'_>, _ws: &mut [MaybeUninit<u8>],
                   window_bits: c_int) -> Option<usize> {
        let mut strm = stream(src.as_ptr(), src.len(), dst);
        unsafe {
            if c::inflateInit2_(&mut strm, window_bits, VERSION, STREAM_SIZE) != c::Z_OK as c_int {
                return None;
            }
            let ret = c::inflate(&mut strm, c::Z_FINISH as c_int);
            c::inflateEnd(&mut strm);
            (ret == c::Z_STREAM_END as c_int).then_some(strm.avail_out as usize)
        }
    }
}

pub mod zstd {
    use crate::c;
    use crate::util::volatile::VolatileSlice;
    use core::ffi::c_int;
    use core::marker::PhantomData;
    use core::mem::MaybeUninit;
    use core::ptr::NonNull;

    pub type Params = c::ZSTD_parameters;

    pub fn max_clevel() -> c_int {
        unsafe { c::ZSTD_maxCLevel() }
    }

    pub fn get_params(level: c_int, estimated_src_size: u64) -> Params {
        unsafe { c::ZSTD_getParams(level, estimated_src_size, 0) }
    }

    pub fn cctx_workspace_bound(params: &Params) -> usize {
        unsafe { c::ZSTD_estimateCCtxSize_usingCParams(params.cParams) }
    }

    pub fn dctx_workspace_bound() -> usize {
        unsafe { c::ZSTD_estimateDCtxSize() }
    }

    /// A ZSTD_ErrorCode, if @ret is one.
    fn result(ret: usize) -> Result<usize, u32> {
        if unsafe { c::ZSTD_isError(ret) } != 0 {
            Err(unsafe { c::ZSTD_getErrorCode(ret) })
        } else {
            Ok(ret)
        }
    }

    pub struct Cctx<'ws>(NonNull<c::ZSTD_CCtx>, PhantomData<&'ws mut [MaybeUninit<u8>]>);

    impl<'ws> Cctx<'ws> {
        /// None if @ws is too small.
        pub fn new(ws: &'ws mut [MaybeUninit<u8>]) -> Option<Self> {
            let p = unsafe { c::ZSTD_initStaticCCtx(ws.as_mut_ptr().cast(), ws.len()) };
            NonNull::new(p).map(|p| Cctx(p, PhantomData))
        }

        /// As the kernel's zstd_compress_cctx(): @params set, then compressed
        /// in one.
        pub fn compress(&mut self, dst: VolatileSlice<'_>, src: &[u8], params: &Params) -> Result<usize, u32> {
            let (cctx, cp, fp) = (self.0.as_ptr(), &params.cParams, &params.fParams);
            let set = [
                (c::ZSTD_cParameter_ZSTD_c_windowLog,       cp.windowLog as c_int),
                (c::ZSTD_cParameter_ZSTD_c_hashLog,         cp.hashLog as c_int),
                (c::ZSTD_cParameter_ZSTD_c_chainLog,        cp.chainLog as c_int),
                (c::ZSTD_cParameter_ZSTD_c_searchLog,       cp.searchLog as c_int),
                (c::ZSTD_cParameter_ZSTD_c_minMatch,        cp.minMatch as c_int),
                (c::ZSTD_cParameter_ZSTD_c_targetLength,    cp.targetLength as c_int),
                (c::ZSTD_cParameter_ZSTD_c_strategy,        cp.strategy as c_int),
                (c::ZSTD_cParameter_ZSTD_c_contentSizeFlag, fp.contentSizeFlag),
                (c::ZSTD_cParameter_ZSTD_c_checksumFlag,    fp.checksumFlag),
                (c::ZSTD_cParameter_ZSTD_c_dictIDFlag,      (fp.noDictIDFlag == 0) as c_int),
            ];
            unsafe {
                result(c::ZSTD_CCtx_reset(cctx, c::ZSTD_ResetDirective_ZSTD_reset_session_and_parameters))?;
                result(c::ZSTD_CCtx_setPledgedSrcSize(cctx, src.len() as u64))?;
                for (param, v) in set {
                    result(c::ZSTD_CCtx_setParameter(cctx, param, v))?;
                }
                result(c::ZSTD_compress2(cctx, dst.as_ptr().cast(), dst.len(), src.as_ptr().cast(), src.len()))
            }
        }
    }

    pub struct Dctx<'ws>(NonNull<c::ZSTD_DCtx>, PhantomData<&'ws mut [MaybeUninit<u8>]>);

    impl<'ws> Dctx<'ws> {
        /// None if @ws is too small.
        pub fn new(ws: &'ws mut [MaybeUninit<u8>]) -> Option<Self> {
            let p = unsafe { c::ZSTD_initStaticDCtx(ws.as_mut_ptr().cast(), ws.len()) };
            NonNull::new(p).map(|p| Dctx(p, PhantomData))
        }

        pub fn decompress(&mut self, dst: VolatileSlice<'_>, src: VolatileSlice<'_>) -> Result<usize, u32> {
            result(unsafe {
                c::ZSTD_decompressDCtx(self.0.as_ptr(), dst.as_ptr().cast(), dst.len(),
                                       src.as_ptr().cast_const().cast(), src.len())
            })
        }
    }
}
