// SPDX-License-Identifier: GPL-2.0

//! The kernel's lz4, zlib and zstd APIs over liblz4, libz and libzstd. lz4 and
//! zlib allocate their own workspaces - theirs are 0 bytes, and the caller's go
//! unused; zstd's contexts are made in the caller's, as the kernel's are.

fn int(n: usize) -> i32 {
    n.try_into().unwrap_or_else(|_| panic!("a codec's length of {n}: past a c_int"))
}

pub mod lz4 {
    use super::int;
    use crate::c;
    use crate::util::volatile::{Out, VolatileSlice};
    use core::mem::MaybeUninit;

    pub const MEM_COMPRESS: usize = 0;
    pub const HC_MEM_COMPRESS: usize = 0;
    /// The kernel's threshold for lz4 against lz4hc.
    pub const HC_MIN_CLEVEL: u32 = 3;

    /// As much of @src as fits in @dst, compressed - None on error - and how
    /// much of @src that was.
    pub fn compress_dest_size<'a>(src: &[u8], dst: Out<'a>, _ws: &mut [MaybeUninit<u8>])
        -> (Option<VolatileSlice<'a>>, usize)
    {
        let mut src_len = int(src.len());
        unsafe {
            let ret = c::LZ4_compress_destSize(src.as_ptr().cast(), dst.as_ptr().cast(), &mut src_len,
                                               int(dst.len()));
            ((ret > 0).then(|| dst.sub(0..ret as usize).assume_init()), src_len as usize)
        }
    }

    /// @src, compressed in @dst - or None if it didn't fit.
    pub fn compress_hc<'a>(src: &[u8], dst: Out<'a>, level: u32, _ws: &mut [MaybeUninit<u8>])
        -> Option<VolatileSlice<'a>>
    {
        unsafe {
            let ret = c::LZ4_compress_HC(src.as_ptr().cast(), dst.as_ptr().cast(), int(src.len()), int(dst.len()),
                                         level as i32);
            (ret > 0).then(|| dst.sub(0..ret as usize).assume_init())
        }
    }

    /// At least @target bytes of @src, decompressed in @dst - or None if @src
    /// is malformed.
    pub fn decompress_safe_partial<'a>(src: VolatileSlice<'_>, dst: Out<'a>, target: usize)
        -> Option<VolatileSlice<'a>>
    {
        unsafe {
            let ret = c::LZ4_decompress_safe_partial(src.as_ptr().cast_const().cast(), dst.as_ptr().cast(),
                                                     int(src.len()), int(target), int(dst.len()));
            Some(dst.sub(0..ret.try_into().ok()?).assume_init())
        }
    }
}

pub mod zlib {
    use super::int;
    use crate::c::{self, z_stream};
    use crate::util::volatile::{Out, VolatileSlice};
    use core::mem::MaybeUninit;

    /// zlib's default, which it keeps to itself: zutil.h's
    pub const DEF_MEM_LEVEL: i32 = 8;

    const VERSION: *const core::ffi::c_char = c::ZLIB_VERSION.as_ptr().cast();
    const STREAM_SIZE: i32 = size_of::<z_stream>() as i32;

    pub fn deflate_workspace_size(_window_bits: i32, _mem_level: i32) -> usize {
        0
    }

    pub fn inflate_workspace_size() -> usize {
        0
    }

    fn stream(src: *const u8, src_len: usize, dst: Out<'_>) -> z_stream {
        z_stream {
            next_in:   src.cast_mut().cast(),
            avail_in:  int(src_len) as _,
            next_out:  dst.as_ptr().cast(),
            avail_out: int(dst.len()) as _,
            ..Default::default()
        }
    }

    /// @src deflated in @dst, all of it - or None if it didn't fit.
    pub fn deflate<'a>(src: &[u8], dst: Out<'a>, _ws: &mut [MaybeUninit<u8>], level: i32,
                       window_bits: i32, mem_level: i32, strategy: i32) -> Option<VolatileSlice<'a>> {
        let mut strm = stream(src.as_ptr(), src.len(), dst);
        unsafe {
            if c::deflateInit2_(&mut strm, level, c::Z_DEFLATED as i32, window_bits, mem_level, strategy,
                                VERSION, STREAM_SIZE) != c::Z_OK as i32 {
                return None;
            }
            let done = c::deflate(&mut strm, c::Z_FINISH as i32) == c::Z_STREAM_END as i32;
            (c::deflateEnd(&mut strm) == c::Z_OK as i32 && done)
                .then(|| dst.sub(0..strm.total_out as usize).assume_init())
        }
    }

    /// @src inflated in @dst - or None if @src is malformed or didn't end.
    pub fn inflate<'a>(src: VolatileSlice<'_>, dst: Out<'a>, _ws: &mut [MaybeUninit<u8>],
                       window_bits: i32) -> Option<VolatileSlice<'a>> {
        let mut strm = stream(src.as_ptr(), src.len(), dst);
        unsafe {
            if c::inflateInit2_(&mut strm, window_bits, VERSION, STREAM_SIZE) != c::Z_OK as i32 {
                return None;
            }
            let ret = c::inflate(&mut strm, c::Z_FINISH as i32);
            c::inflateEnd(&mut strm);
            (ret == c::Z_STREAM_END as i32).then(|| dst.sub(0..dst.len() - strm.avail_out as usize).assume_init())
        }
    }
}

pub mod zstd {
    use crate::c;
    use crate::util::volatile::{Out, VolatileSlice};
    use core::marker::PhantomData;
    use core::mem::MaybeUninit;
    use core::ptr::NonNull;

    pub type Params = c::ZSTD_parameters;

    pub fn max_clevel() -> i32 {
        unsafe { c::ZSTD_maxCLevel() }
    }

    pub fn get_params(level: i32, estimated_src_size: u64) -> Params {
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
        pub fn compress<'a>(&mut self, dst: Out<'a>, src: &[u8], params: &Params) -> Result<VolatileSlice<'a>, u32> {
            let (cctx, cp, fp) = (self.0.as_ptr(), &params.cParams, &params.fParams);
            let set = [
                (c::ZSTD_cParameter_ZSTD_c_windowLog,       cp.windowLog as i32),
                (c::ZSTD_cParameter_ZSTD_c_hashLog,         cp.hashLog as i32),
                (c::ZSTD_cParameter_ZSTD_c_chainLog,        cp.chainLog as i32),
                (c::ZSTD_cParameter_ZSTD_c_searchLog,       cp.searchLog as i32),
                (c::ZSTD_cParameter_ZSTD_c_minMatch,        cp.minMatch as i32),
                (c::ZSTD_cParameter_ZSTD_c_targetLength,    cp.targetLength as i32),
                (c::ZSTD_cParameter_ZSTD_c_strategy,        cp.strategy as i32),
                (c::ZSTD_cParameter_ZSTD_c_contentSizeFlag, fp.contentSizeFlag),
                (c::ZSTD_cParameter_ZSTD_c_checksumFlag,    fp.checksumFlag),
                (c::ZSTD_cParameter_ZSTD_c_dictIDFlag,      (fp.noDictIDFlag == 0) as i32),
            ];
            unsafe {
                result(c::ZSTD_CCtx_reset(cctx, c::ZSTD_ResetDirective_ZSTD_reset_session_and_parameters))?;
                result(c::ZSTD_CCtx_setPledgedSrcSize(cctx, src.len() as u64))?;
                for (param, v) in set {
                    result(c::ZSTD_CCtx_setParameter(cctx, param, v))?;
                }
                let len = result(c::ZSTD_compress2(cctx, dst.as_ptr().cast(), dst.len(),
                                                   src.as_ptr().cast(), src.len()))?;
                Ok(dst.sub(0..len).assume_init())
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

        /// @src, decompressed in @dst.
        pub fn decompress<'a>(&mut self, dst: Out<'a>, src: VolatileSlice<'_>) -> Result<VolatileSlice<'a>, u32> {
            unsafe {
                let len = result(c::ZSTD_decompressDCtx(self.0.as_ptr(), dst.as_ptr().cast(), dst.len(),
                                                        src.as_ptr().cast_const().cast(), src.len()))?;
                Ok(dst.sub(0..len).assume_init())
            }
        }
    }
}
