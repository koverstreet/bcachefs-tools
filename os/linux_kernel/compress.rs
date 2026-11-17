// SPDX-License-Identifier: GPL-2.0

//! The kernel's lz4, zlib and zstd, each in a workspace the caller has.

fn int(n: usize) -> core::ffi::c_int {
    n.try_into().unwrap_or_else(|_| panic!("a codec's length of {n}: past a c_int"))
}

pub mod lz4 {
    use super::int;
    use crate::c;
    use crate::util::volatile::{Out, VolatileSlice};
    use core::mem::MaybeUninit;

    pub const MEM_COMPRESS: usize = c::LZ4_MEM_COMPRESS as usize;
    pub const HC_MEM_COMPRESS: usize = c::LZ4HC_MEM_COMPRESS as usize;
    pub const HC_MIN_CLEVEL: u32 = c::LZ4HC_MIN_CLEVEL as u32;

    /// As much of @src as fits in @dst, compressed - None on error - and how
    /// much of @src that was.
    pub fn compress_dest_size<'a>(src: &[u8], dst: Out<'a>, ws: &mut [MaybeUninit<u8>])
        -> (Option<VolatileSlice<'a>>, usize)
    {
        assert!(ws.len() >= MEM_COMPRESS, "lz4 workspace of {} bytes, not {MEM_COMPRESS}", ws.len());
        let mut src_len = int(src.len());
        unsafe {
            let ret = c::LZ4_compress_destSize(src.as_ptr().cast(), dst.as_ptr().cast(), &mut src_len,
                                               int(dst.len()), ws.as_mut_ptr().cast());
            ((ret > 0).then(|| dst.sub(0..ret as usize).assume_init()), src_len as usize)
        }
    }

    /// @src, compressed in @dst - or None if it didn't fit.
    pub fn compress_hc<'a>(src: &[u8], dst: Out<'a>, level: u32, ws: &mut [MaybeUninit<u8>])
        -> Option<VolatileSlice<'a>>
    {
        assert!(ws.len() >= HC_MEM_COMPRESS, "lz4hc workspace of {} bytes, not {HC_MEM_COMPRESS}", ws.len());
        unsafe {
            let ret = c::LZ4_compress_HC(src.as_ptr().cast(), dst.as_ptr().cast(), int(src.len()), int(dst.len()),
                                         level as _, ws.as_mut_ptr().cast());
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
    use crate::c;
    use crate::util::volatile::{Out, VolatileSlice};
    use core::ffi::c_int;
    use core::mem::MaybeUninit;

    pub const DEF_MEM_LEVEL: c_int = c::DEF_MEM_LEVEL as c_int;

    pub fn deflate_workspace_size(window_bits: c_int, mem_level: c_int) -> usize {
        unsafe { c::zlib_deflate_workspacesize(window_bits, mem_level) as usize }
    }

    pub fn inflate_workspace_size() -> usize {
        unsafe { c::zlib_inflate_workspacesize() as usize }
    }

    fn stream(src: *const u8, src_len: usize, dst: Out<'_>, ws: &mut [MaybeUninit<u8>]) -> c::z_stream {
        c::z_stream {
            next_in:   src.cast(),
            avail_in:  int(src_len) as _,
            next_out:  dst.as_ptr().cast(),
            avail_out: int(dst.len()) as _,
            workspace: ws.as_mut_ptr().cast(),
            ..Default::default()
        }
    }

    /// @src deflated in @dst, all of it - or None if it didn't fit.
    pub fn deflate<'a>(src: &[u8], dst: Out<'a>, ws: &mut [MaybeUninit<u8>], level: c_int,
                       window_bits: c_int, mem_level: c_int, strategy: c_int) -> Option<VolatileSlice<'a>> {
        assert!(ws.len() >= deflate_workspace_size(window_bits, mem_level),
                "zlib deflate workspace of {} bytes, not {}", ws.len(), deflate_workspace_size(window_bits, mem_level));
        let mut strm = stream(src.as_ptr(), src.len(), dst, ws);
        unsafe {
            if c::zlib_deflateInit2(&mut strm, level, c::Z_DEFLATED as c_int, window_bits, mem_level, strategy)
                != c::Z_OK as c_int {
                return None;
            }
            let done = c::zlib_deflate(&mut strm, c::Z_FINISH as c_int) == c::Z_STREAM_END as c_int;
            (c::zlib_deflateEnd(&mut strm) == c::Z_OK as c_int && done)
                .then(|| dst.sub(0..strm.total_out as usize).assume_init())
        }
    }

    /// @src inflated in @dst - or None if @src is malformed or didn't end.
    pub fn inflate<'a>(src: VolatileSlice<'_>, dst: Out<'a>, ws: &mut [MaybeUninit<u8>],
                       window_bits: c_int) -> Option<VolatileSlice<'a>> {
        assert!(ws.len() >= inflate_workspace_size(),
                "zlib inflate workspace of {} bytes, not {}", ws.len(), inflate_workspace_size());
        let mut strm = stream(src.as_ptr(), src.len(), dst, ws);
        unsafe {
            if c::zlib_inflateInit2(&mut strm, window_bits) != c::Z_OK as c_int {
                return None;
            }
            let ret = c::zlib_inflate(&mut strm, c::Z_FINISH as c_int);
            c::zlib_inflateEnd(&mut strm);
            (ret == c::Z_STREAM_END as c_int).then(|| dst.sub(0..dst.len() - strm.avail_out as usize).assume_init())
        }
    }
}

pub mod zstd {
    use crate::c;
    use crate::util::volatile::{Out, VolatileSlice};
    use core::ffi::c_int;
    use core::marker::PhantomData;
    use core::mem::MaybeUninit;
    use core::ptr::NonNull;

    pub type Params = c::zstd_parameters;

    pub fn max_clevel() -> c_int {
        unsafe { c::zstd_max_clevel() }
    }

    pub fn get_params(level: c_int, estimated_src_size: u64) -> Params {
        unsafe { c::zstd_get_params(level, estimated_src_size) }
    }

    pub fn cctx_workspace_bound(params: &Params) -> usize {
        unsafe { c::zstd_cctx_workspace_bound(&params.cParams) }
    }

    pub fn dctx_workspace_bound() -> usize {
        unsafe { c::zstd_dctx_workspace_bound() }
    }

    /// A zstd_error_code, if @ret is one.
    fn result(ret: usize) -> Result<usize, u32> {
        if unsafe { c::zstd_is_error(ret) } != 0 {
            Err(unsafe { c::zstd_get_error_code(ret) })
        } else {
            Ok(ret)
        }
    }

    pub struct Cctx<'ws>(NonNull<c::zstd_cctx>, PhantomData<&'ws mut [MaybeUninit<u8>]>);

    impl<'ws> Cctx<'ws> {
        /// None if @ws is too small.
        pub fn new(ws: &'ws mut [MaybeUninit<u8>]) -> Option<Self> {
            let p = unsafe { c::zstd_init_cctx(ws.as_mut_ptr().cast(), ws.len()) };
            NonNull::new(p).map(|p| Cctx(p, PhantomData))
        }

        /// @src, compressed in @dst.
        pub fn compress<'a>(&mut self, dst: Out<'a>, src: &[u8], params: &Params) -> Result<VolatileSlice<'a>, u32> {
            unsafe {
                let len = result(c::zstd_compress_cctx(self.0.as_ptr(), dst.as_ptr().cast(), dst.len(),
                                                       src.as_ptr().cast(), src.len(), params))?;
                Ok(dst.sub(0..len).assume_init())
            }
        }
    }

    pub struct Dctx<'ws>(NonNull<c::zstd_dctx>, PhantomData<&'ws mut [MaybeUninit<u8>]>);

    impl<'ws> Dctx<'ws> {
        /// None if @ws is too small.
        pub fn new(ws: &'ws mut [MaybeUninit<u8>]) -> Option<Self> {
            let p = unsafe { c::zstd_init_dctx(ws.as_mut_ptr().cast(), ws.len()) };
            NonNull::new(p).map(|p| Dctx(p, PhantomData))
        }

        /// @src, decompressed in @dst.
        pub fn decompress<'a>(&mut self, dst: Out<'a>, src: VolatileSlice<'_>) -> Result<VolatileSlice<'a>, u32> {
            unsafe {
                let len = result(c::zstd_decompress_dctx(self.0.as_ptr(), dst.as_ptr().cast(), dst.len(),
                                                         src.as_ptr().cast_const().cast(), src.len()))?;
                Ok(dst.sub(0..len).assume_init())
            }
        }
    }
}
