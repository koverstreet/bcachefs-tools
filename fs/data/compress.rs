// SPDX-License-Identifier: GPL-2.0

/* DOC(compression)
 *
 * Compression is set per-file or per-directory and operates at extent
 * granularity (up to 128K). Supported algorithms: lz4 (fast, moderate ratio),
 * zstd (good ratio, moderate CPU), gzip (best ratio, highest CPU). Levels are
 * configurable: e.g. `compression=zstd:3` (range 1-15). Higher levels improve
 * ratio at the cost of write CPU; read CPU is determined by the data, not the
 * original compression level.
 *
 * The main tradeoff to be aware of: small random reads within a compressed
 * extent must read and decompress the entire extent (up to 128K) because the
 * checksum covers the whole extent. For workloads dominated by small random
 * reads (e.g. virtual machine images or databases), compression may hurt read
 * performance. Sequential reads and large-block random reads are largely
 * unaffected.
 *
 * The `encoded_extent_max` option bounds the amount of data that a small
 * compressed read may have to pull in and decode. Lower values reduce random
 * read amplification at the cost of compression ratio and metadata efficiency.
 * Random-read-heavy workloads should prefer lz4 and/or a smaller
 * `encoded_extent_max` over high-level zstd with large encoded extents.
 *
 * Data can be recompressed in the background with a different algorithm or
 * level via the `background_compression` option — for example, writing with
 * fast lz4 and recompressing to zstd offline. The reconcile subsystem handles
 * this automatically.
 */

//! An opt is a compression option's value, a type and a level; an extent's
//! type is an opt's, or incompressible, or lz4_old. A bounce buffer - C's
//! struct bbuf, which write.c holds too - is an extent's data as one buffer:
//! the bio's own memory if it's virtually contiguous or can be vmapped, else
//! a copy. A bio's memory is only ever a pointer here: it can change under us
//! (util/bio/iter.rs).
//!
//! Buffers and workspaces come from the allocator when it has them cheaply,
//! else from mempools, made as each type is enabled and kept: a mempool can't
//! grow. A type's compression pool being there is its being marked in the
//! superblock.

use crate::c::{self, bbuf_type as bb, bch_compression_opt, bch_compression_opts as opts,
               bch_compression_type as ty, bch_errcode as e};
use crate::{bch_err, btree::iter::lockrestart_do, btree_trans, fs::Fs, fsck_err};
use crate::errcode::{ret_to_c, BchError};
use crate::init::error::{count_fsck_err, id};
use crate::sb::errors::bch2_decompress_sb_err;
use crate::util::{alloc::{flags::{GFP_KERNEL, GFP_NOWAIT, __GFP_NOWARN}, kmalloc, KVVec}, bio::{Bio, BioRange},
                  ffi::Opaque, kernel::{unlikely, READ, WRITE}, log::LogMsg, mempool::Mempool,
                  volatile::VolatileSlice, Printbuf, ToText};
use core::ffi::{c_int, c_uint, c_void};
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use cstruct_macros::c_export;

crate::module_param! {
    pub(crate) static verify_compress: bool = cfg!(CONFIG_BCACHEFS_DEBUG);
    perm = 0o644, desc = "Decompress data immediately after compressing, and verify the result";
}

const OPT_NR: usize = opts::BCH_COMPRESSION_OPT_NR.0 as usize;


/// The opt whose pools decompress type @t.
fn type_to_opt(t: ty) -> usize {
    match t {
        ty::BCH_COMPRESSION_TYPE_none | ty::BCH_COMPRESSION_TYPE_incompressible => opts::BCH_COMPRESSION_OPT_none,
        ty::BCH_COMPRESSION_TYPE_lz4_old | ty::BCH_COMPRESSION_TYPE_lz4 => opts::BCH_COMPRESSION_OPT_lz4,
        ty::BCH_COMPRESSION_TYPE_gzip => opts::BCH_COMPRESSION_OPT_gzip,
        ty::BCH_COMPRESSION_TYPE_zstd => opts::BCH_COMPRESSION_OPT_zstd,
        _ => panic!("compression type {}", t.0),
    }.0 as usize
}

/// Opt type @t's superblock feature. None's is lz4's, bit 0: compression=none
/// still gets lz4's pools at mount.
fn opt_feature(t: usize) -> u64 {
    macro_rules! map {
        ((none, $n0:literal), $(($t:ident, $n:literal)),* $(,)?) => { ::paste::paste! {
            match t {
                $n0 => 1,
                $($n => 1 << c::bch_sb_feature::[<BCH_FEATURE_ $t>].0,)*
                _ => panic!("compression opt type {t}"),
            }
        } };
    }
    c::BCH_COMPRESSION_OPTS!(map)
}

fn zstd_errcode(code: u32) -> e {
    macro_rules! map {
        ($(($n:ident, $nr:literal)),* $(,)?) => { ::paste::paste! {
            match code {
                $(c::[<ZSTD_ErrorCode_ZSTD_error_ $n>] => e::[<BCH_ERR_ZSTD_error_ $n>],)*
                _ => e::BCH_ERR_ZSTD_error_unknown,
            }
        } };
    }
    c::ZSTD_ERRS!(map)
}

fn extent_max(fs: &Fs) -> usize {
    fs.opts().encoded_extent_max.get() as usize
}

/// Compressing and decompressing want wildly different amounts of scratch
/// space, so they get separate pools: a zstd compression context carries the
/// match finder's hash and chain tables, sized by the compression level, while
/// decompression carries only the huffman and FSE decode tables - it doesn't
/// search, it follows offsets. One pool sized to the larger would make every
/// read pay the write side's footprint. lz4 decompresses with no workspace.
#[derive(::pin_init::Zeroable)]
pub struct Compress {
    bounce:     [Mempool; 2],
    workspace:  [Mempool; OPT_NR],
    decompress: [Mempool; OPT_NR],
}

// ── Bounce buffers ───────────────────────────────────────────────────────

/// Freed on drop, unless given to C. 'a is what it borrows: the bio whose
/// pages it maps, or the filesystem whose pool it's from.
struct Bbuf<'a> {
    raw: c::bbuf,
    len: usize,
    _a:  PhantomData<&'a ()>,
}

impl<'a> Bbuf<'a> {
    fn new(fs: &'a Fs, b: NonNull<u8>, type_: bb, rw: usize, len: usize) -> Self {
        Bbuf { raw: c::bbuf { c: fs.as_ptr(), b: b.as_ptr().cast(), type_, rw: rw as c_int }, len, _a: PhantomData }
    }

    /// Ours, or the bio's - which may be written meanwhile.
    fn data(&self) -> VolatileSlice<'_> {
        // SAFETY: @len bytes, until it's dropped
        unsafe { VolatileSlice::from_raw(NonNull::new(self.raw.b.cast()).expect("a bounce buffer"), self.len) }
    }

    /// The bio's own memory: nothing to copy back.
    fn mapped(&self) -> bool {
        matches!(self.raw.type_, bb::BB_none | bb::BB_vmap)
    }

    fn into_c(self) -> c::bbuf {
        core::mem::ManuallyDrop::new(self).raw
    }
}

impl Drop for Bbuf<'_> {
    fn drop(&mut self) {
        // SAFETY: ours
        unsafe { bch2_bbuf_exit(&mut self.raw) }
    }
}

/// # Safety
/// @buf is one of ours, not used after.
#[c_export]
pub unsafe fn bch2_bbuf_exit(buf: &mut c::bbuf) {
    let Some(p) = NonNull::new(buf.b.cast::<u8>()) else { return };
    // SAFETY: the caller's; its type is how it was allocated
    unsafe {
        match buf.type_ {
            bb::BB_none => {}
            #[cfg(kernel)]
            bb::BB_vmap => c::rust_vunmap(p.as_ptr().map_addr(|a| a & !(crate::util::kernel::PAGE_SIZE - 1)).cast()),
            #[cfg(not(kernel))]
            bb::BB_vmap => unreachable!(),
            bb::BB_kmalloc => c::rust_kfree(p.as_ptr().cast()),
            bb::BB_mempool => Fs::borrow_raw(buf.c).compress.bounce[buf.rw as usize].free_raw(p),
        }
    }
}

fn bounce_alloc(fs: &Fs, size: usize, rw: usize) -> Bbuf<'_> {
    assert!(size <= extent_max(fs), "bounce buffer of {size} bytes, past encoded_extent_max {}", extent_max(fs));

    /*
     * Not zeroed - kmalloc()'s, or a mempool element reused - and it needn't
     * be: every bounce is either fully overwritten before it is read, or its
     * unwritten tail is never consumed. By case:
     *
     *  - READ bounces (compression input, decompression compressed-input):
     *    bio_bounce() copies the entire buffer from the bio before anything
     *    reads it.
     *
     *  - decompression output: buf_uncompress() verifies it produced the
     *    full uncompressed_size (lz4/zstd check the returned length, gzip
     *    checks avail_out); on a decompress error the buffer is discarded,
     *    not consumed.
     *
     *  - compression output: the compressor fills the compressed data and
     *    compress() zeroes the block-alignment pad. The output bio is
     *    trimmed to the compressed size before submission, so the unwritten
     *    tail past it (copied into the bio, but never submitted) does not
     *    reach disk.
     *
     * The one hole is no_data_io, which can hand an unwritten buffer to
     * userspace - but it is root-only and explicitly unsafe, so we ignore
     * it.
     */
    match kmalloc(size, GFP_KERNEL | __GFP_NOWARN) {
        Ok(p) => Bbuf::new(fs, p, bb::BB_kmalloc, rw, size),
        Err(_) => Bbuf::new(fs, fs.compress.bounce[rw].alloc_raw(), bb::BB_mempool, rw, size),
    }
}

fn bio_bounce<'f>(fs: &'f Fs, r: BioRange<'_>, rw: usize) -> Bbuf<'f> {
    let b = bounce_alloc(fs, r.len() as usize, rw);
    if rw == READ {
        r.copy_to_volatile(b.data());
    }
    b
}

/// Where @r starts, if it's virtually contiguous: no HIGHMEM, so every
/// chunk's in the direct map.
#[cfg(not(CONFIG_HIGHMEM))]
fn virt_contig(r: BioRange<'_>) -> Option<NonNull<u8>> {
    let mut chunks = r.into_iter().map(|c| (c.with_mapped(|p, _| p), c.len() as usize));
    let (start, len) = chunks.next()?;
    let mut end = start.wrapping_add(len);
    for (p, len) in chunks {
        if p != end {
            return None;
        }
        end = p.wrapping_add(len);
    }
    NonNull::new(start)
}

/// @r's pages mapped as one - if it's whole pages but where it starts and
/// ends.
#[cfg(kernel)]
fn vmap<'a>(fs: &'a Fs, r: BioRange<'a>, rw: usize) -> Option<Bbuf<'a>> {
    use crate::util::bio::{nth_page, Segment};
    use crate::util::kernel::PAGE_SIZE;

    let pages_of = |s: &Segment<'_>| ((s.offset + s.len) as usize).div_ceil(PAGE_SIZE);
    let offset = r.segments().next()?.offset as usize;
    let mut seen = 0;
    for s in r.segments() {
        if (seen != 0 && s.offset != 0) ||
           (seen + s.len != r.len() && (s.offset + s.len) as usize % PAGE_SIZE != 0) {
            return None;
        }
        seen += s.len;
    }
    let nr = (offset + r.len() as usize).div_ceil(PAGE_SIZE);

    let mut stack = [ptr::null_mut::<c::page>(); 16];
    let mut heap;
    let pages: &mut [*mut c::page] = if nr <= stack.len() {
        &mut stack[..nr]
    } else {
        heap = KVVec::from_elem(ptr::null_mut(), nr, GFP_KERNEL).ok()?;
        &mut heap
    };

    let mut i = 0;
    for s in r.segments() {
        for n in 0..pages_of(&s) {
            // SAFETY: the segment's pages
            pages[i] = unsafe { nth_page(s.page.as_ptr(), n) };
            i += 1;
        }
    }

    // SAFETY: nr of the bio's pages
    let data = unsafe { c::rust_vmap(pages.as_mut_ptr(), nr as c_uint) };
    // SAFETY: within its first page
    Some(Bbuf::new(fs, unsafe { NonNull::new(data.cast::<u8>())?.add(offset) }, bb::BB_vmap, rw, r.len() as usize))
}

fn map_or_bounce<'a>(fs: &'a Fs, r: BioRange<'a>, rw: usize) -> Bbuf<'a> {
    assert!(r.len() as usize <= extent_max(fs), "mapping {} bytes, past encoded_extent_max {}",
            r.len(), extent_max(fs));

    #[cfg(not(CONFIG_HIGHMEM))]
    if let Some(p) = virt_contig(r) {
        return Bbuf::new(fs, p, bb::BB_none, rw, r.len() as usize);
    }
    #[cfg(kernel)]
    if let Some(b) = vmap(fs, r, rw) {
        return b;
    }
    bio_bounce(fs, r, rw)
}

#[c_export]
pub fn bch2_bounce_alloc(c: &Opaque<c::bch_fs>, size: c_uint, rw: c_int) -> c::bbuf {
    bounce_alloc(Fs::from_c(c), size as usize, rw as usize).into_c()
}

/// # Safety
/// @start is a position of @bio.
#[c_export]
pub unsafe fn bch2_bio_bounce(c: &Opaque<c::bch_fs>, bio: *mut c::bio, start: c::bvec_iter, rw: c_int) -> c::bbuf {
    // SAFETY: the caller's
    bio_bounce(Fs::from_c(c), unsafe { Bio::from_raw(bio).range_at(start) }, rw as usize).into_c()
}

/// # Safety
/// @bio is a bio.
#[c_export]
pub unsafe fn bch2_bio_map_or_bounce(c: &Opaque<c::bch_fs>, bio: *mut c::bio, rw: c_int) -> c::bbuf {
    // SAFETY: the caller's
    map_or_bounce(Fs::from_c(c), unsafe { Bio::from_raw(bio).range() }, rw as usize).into_c()
}

// ── Decompression ────────────────────────────────────────────────────────

/// In @ws - which libz, in userspace, has no field for: it allocates its own.
fn z_stream(src: *const u8, src_len: usize, dst: *mut u8, dst_len: usize,
            #[cfg_attr(not(kernel), allow(unused))] ws: *mut c_void) -> c::z_stream {
    c::z_stream {
        next_in: src as _, avail_in: src_len as _,
        next_out: dst.cast(), avail_out: dst_len as _,
        #[cfg(kernel)]
        workspace: ws,
        ..Default::default()
    }
}

fn buf_uncompress(fs: &Fs, dst: VolatileSlice<'_>, src: VolatileSlice<'_>, crc: &c::bch_extent_crc_unpacked)
    -> Result<(), BchError>
{
    let t = ty(crc.compression_type.into());
    let opt = type_to_opt(t);

    /*
     * The compression pool is what tells us the type is marked in the
     * superblock: both pools are set up together, and lz4 needs no
     * decompression workspace at all, so its decompress pool is never
     * initialized and can't stand in for the check.
     */
    if unlikely(!fs.compress.workspace[opt].initialized()) {
        if !fsck_err!(fs, id::compression_type_not_marked_in_sb,
                      "compression type {} set but not marked in superblock", t)? {
            return fs.throw(e::BCH_ERR_compression_workspace_not_initialized);
        }
        check_set_has_compressed_data(fs, opt as u8)?;
    }

    let (src_len, dst_len) = ((crc.compressed_size as usize) << 9, (crc.uncompressed_size as usize) << 9);
    assert!(src.len() >= src_len && dst.len() >= dst_len && !src.overlaps(&dst),
            "decompressing {src_len} bytes from a buffer of {} into {dst_len} of {}, or overlapping",
            src.len(), dst.len());
    let (src, dst) = (src.as_ptr().cast_const(), dst.as_ptr());
    // none for lz4, and for gzip in userspace, whose workspace is 0 bytes
    let pool = &fs.compress.decompress[opt];
    let ws = pool.initialized().then(|| pool.alloc());
    let ws = ws.as_ref().map_or(ptr::null_mut(), |b| b.as_ptr().cast());

    // SAFETY: @src and @dst crc's sizes, apart
    unsafe {
        match t {
            ty::BCH_COMPRESSION_TYPE_lz4_old | ty::BCH_COMPRESSION_TYPE_lz4 => {
                let (s, d) = (src_len as c_int, dst_len as c_int);
                if c::LZ4_decompress_safe_partial(src.cast(), dst.cast(), s, d, d) != d {
                    return fs.throw(if t == ty::BCH_COMPRESSION_TYPE_lz4 { e::BCH_ERR_decompress_lz4 }
                                    else { e::BCH_ERR_decompress_lz4_old });
                }
            }
            ty::BCH_COMPRESSION_TYPE_gzip => {
                let mut strm = z_stream(src, src_len, dst, dst_len, ws);
                c::zlib_inflateInit2(&mut strm, -(c::MAX_WBITS as c_int));
                let ret = c::zlib_inflate(&mut strm, c::Z_FINISH as c_int);
                c::zlib_inflateEnd(&mut strm);

                if ret != c::Z_STREAM_END as c_int {
                    return fs.throw(e::BCH_ERR_decompress_gzip);
                }
                if strm.avail_out != 0 {
                    return fs.throw(e::BCH_ERR_decompress_gzip_size_mismatch);
                }
            }
            ty::BCH_COMPRESSION_TYPE_zstd => {
                let real_src_len = match src_len {
                    4.. => u32::from_le_bytes(src.cast::<[u8; 4]>().read_unaligned()) as usize,
                    _ => usize::MAX,
                };
                if real_src_len > src_len.saturating_sub(4) {
                    return fs.throw(e::BCH_ERR_decompress_zstd_src_len_bad);
                }

                let ctx = c::zstd_init_dctx(ws, c::zstd_dctx_workspace_bound());
                let ret = c::zstd_decompress_dctx(ctx, dst.cast(), dst_len, src.add(4).cast(), real_src_len);
                if ret != dst_len {
                    if c::zstd_is_error(ret) != 0 {
                        return fs.throw(zstd_errcode(c::zstd_get_error_code(ret)));
                    }
                    bch_err!(fs, "zstd decompress error: expected {dst_len} bytes, got {ret}");
                    return fs.throw(e::BCH_ERR_decompress_zstd_size_mismatch);
                }
            }
            _ => panic!("decompressing compression type {}", t.0),
        }
    }
    Ok(())
}

/// # Safety
/// @dst and @src are @crc's uncompressed and compressed sizes.
#[c_export]
pub unsafe fn bch2_buf_uncompress(c: &Opaque<c::bch_fs>, dst: *mut c_void, src: *mut c_void,
                                  crc: c::bch_extent_crc_unpacked) -> c_int {
    let buf = |p: *mut c_void, sectors: u32| {
        // SAFETY: the caller's
        unsafe { VolatileSlice::from_raw(NonNull::new(p.cast()).expect("a buffer"), (sectors as usize) << 9) }
    };
    ret_to_c(buf_uncompress(Fs::from_c(c), buf(dst, crc.uncompressed_size), buf(src, crc.compressed_size), &crc))
}

/*
 * Every failed decompression attempt gets counted, from the read path and
 * from the write path both - the move path decompresses when it can't
 * write the extent through as-is, and that's where a filesystem full of
 * unmovable compressed extents shows itself. Which compression type
 * failed, and for zstd why, is the first thing such a report has to
 * answer.
 *
 * Not under no_data_io, where decompressing whatever was in the buffer is
 * expected to fail and says nothing about the filesystem.
 */
fn decompress_err(fs: &Fs, ret: Result<(), BchError>) -> Result<(), BchError> {
    if let Err(err) = ret {
        if fs.opts().no_data_io.get() == 0 {
            fs.sb_error_count(bch2_decompress_sb_err(err.raw()));
        }
    }
    ret
}

#[c_export]
pub fn bch2_decompress_err(c: &Opaque<c::bch_fs>, ret: c_int) -> c_int {
    let ret = if ret != 0 { Err(BchError::from_raw(-ret)) } else { Ok(()) };
    ret_to_c(decompress_err(Fs::from_c(c), ret))
}

/// # Safety
/// @src and @dst are bios, @dst_iter a position of @dst.
#[c_export]
pub unsafe fn bch2_bio_uncompress(c: &Opaque<c::bch_fs>, src: *mut c::bio, dst: *mut c::bio,
                                  dst_iter: c::bvec_iter, crc: c::bch_extent_crc_unpacked) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: the caller's
    let (src, dst) = unsafe { (Bio::from_raw(src), Bio::from_raw(dst).range_at(dst_iter)) };
    let dst_len = (crc.uncompressed_size as usize) << 9;

    assert!(dst.len() as usize + ((crc.offset as usize) << 9) <= dst_len,
            "decompressing {} bytes at sector {} of an extent of {dst_len}", dst.len(), crc.offset);
    assert_eq!(src.raw().bi_iter.bi_size, crc.compressed_size << 9,
               "decompressing a bio that isn't the compressed extent");

    if (crc.uncompressed_size.max(crc.compressed_size) as usize) << 9 > extent_max(fs) {
        return ret_to_c(decompress_err(fs, fs.throw(e::BCH_ERR_decompress_exceeded_max_encoded_extent)));
    }

    let dst_buf = if dst.len() as usize == dst_len {
        map_or_bounce(fs, dst, WRITE)
    } else {
        bounce_alloc(fs, dst_len, WRITE)
    };
    let src_buf = map_or_bounce(fs, src.range(), READ);

    let ret = decompress_err(fs, buf_uncompress(fs, dst_buf.data(), src_buf.data(), &crc));
    if ret.is_ok() && !dst_buf.mapped() {
        dst.copy_from_volatile(dst_buf.data().sub(((crc.offset as usize) << 9)..dst_len));
    }
    ret_to_c(ret)
}

// ── Compression ──────────────────────────────────────────────────────────

/// Ours go to 15, zstd's to 22.
fn zstd_params(fs: &Fs, level: u32) -> c::zstd_parameters {
    // SAFETY: pure functions
    unsafe { c::zstd_get_params(((level * 3) / 2).min(c::zstd_max_clevel() as u32) as c_int, extent_max(fs) as u64) }
}

/*
 * What this particular compression needs, which is generally much less than
 * the mempool element holds: the pool is sized for the worst case any caller
 * could ask for - highest level, and for lz4 the high-compression variant.
 */
fn compress_workspace_size(fs: &Fs, t: usize, level: u32) -> usize {
    // SAFETY: pure functions
    unsafe {
        match opts(t as u32) {
            opts::BCH_COMPRESSION_OPT_lz4 if level < c::LZ4HC_MIN_CLEVEL => c::LZ4_MEM_COMPRESS as usize,
            opts::BCH_COMPRESSION_OPT_lz4 => c::LZ4HC_MEM_COMPRESS as usize,
            opts::BCH_COMPRESSION_OPT_gzip =>
                c::zlib_deflate_workspacesize(c::MAX_WBITS as c_int, c::DEF_MEM_LEVEL as c_int) as usize,
            opts::BCH_COMPRESSION_OPT_zstd => c::zstd_cctx_workspace_bound(&zstd_params(fs, level).cParams),
            _ => panic!("compression opt type {t}"),
        }
    }
}

/// The compressed length; if it didn't fit, negative - how much of @src
/// would, if the compressor can say - or 0.
fn attempt_compress(fs: &Fs, ws: &mut [MaybeUninit<u8>], dst: VolatileSlice<'_>, src: &[u8], t: usize, level: u32)
    -> isize
{
    /*
     * LZ4HC uses LZ4_wildCopy(), which may overwrite up to 7 bytes past
     * its requested copy end. Keep that inside our bounce buffer even when
     * the compressed output ends exactly at the supplied limit.
     */
    const LZ4HC_OUTPUT_SLOP: usize = 7;

    let (src_len, dst_len, ws_size) = (src.len(), dst.len(), ws.len());
    assert!(src_len % 512 == 0 && dst_len % 512 == 0, "compressing {src_len} bytes into {dst_len}, not sectors");
    let (src, dst, ws) = (src.as_ptr(), dst.as_ptr(), ws.as_mut_ptr().cast::<c_void>());
    let int = |x: usize| x as c_int;

    // SAFETY: each its length, apart
    unsafe {
        match opts(t as u32) {
            opts::BCH_COMPRESSION_OPT_lz4 if level < c::LZ4HC_MIN_CLEVEL => {
                let mut len = int(src_len);
                let ret = c::LZ4_compress_destSize(src.cast(), dst.cast(), &mut len, int(dst_len), ws);
                if (len as usize) < src_len { -(len as isize) } else { ret as isize }
            }
            opts::BCH_COMPRESSION_OPT_lz4 => {
                if dst_len <= LZ4HC_OUTPUT_SLOP {
                    return -1;
                }
                match c::LZ4_compress_HC(src.cast(), dst.cast(), int(src_len),
                                         int(dst_len - LZ4HC_OUTPUT_SLOP), level as c_int, ws) {
                    0 => -1,
                    n => n as isize,
                }
            }
            opts::BCH_COMPRESSION_OPT_gzip => {
                let level = match level {
                    0 => c::Z_DEFAULT_COMPRESSION as c_int,
                    l => (l as c_int).clamp(c::Z_BEST_SPEED as c_int, c::Z_BEST_COMPRESSION as c_int),
                };
                let mut strm = z_stream(src, src_len, dst, dst_len, ws);
                if c::zlib_deflateInit2(&mut strm, level, c::Z_DEFLATED as c_int, -(c::MAX_WBITS as c_int),
                                        c::DEF_MEM_LEVEL as c_int, c::Z_DEFAULT_STRATEGY as c_int) != c::Z_OK as c_int {
                    return 0;
                }
                let done = c::zlib_deflate(&mut strm, c::Z_FINISH as c_int) == c::Z_STREAM_END as c_int;
                if c::zlib_deflateEnd(&mut strm) != c::Z_OK as c_int || !done {
                    return 0;
                }
                strm.total_out as isize
            }
            opts::BCH_COMPRESSION_OPT_zstd => {
                /*
                 * ZSTD requires that when we decompress we pass in the exact
                 * compressed size - rounding it up to the nearest sector
                 * doesn't work, so we use the first 4 bytes of the buffer for
                 * that.
                 *
                 * Additionally, the ZSTD code seems to have a bug where it will
                 * write just past the end of the buffer - so subtract a fudge
                 * factor (7 bytes) from the dst buffer size to account for
                 * that.
                 */
                let ctx = c::zstd_init_cctx(ws, ws_size);
                let len = c::zstd_compress_cctx(ctx, dst.add(4).cast(), dst_len - 4 - 7,
                                                src.cast(), src_len, &zstd_params(fs, level));
                if c::zstd_is_error(len) != 0 {
                    return 0;
                }
                dst.cast::<[u8; 4]>().write_unaligned((len as u32).to_le_bytes());
                (len + 4) as isize
            }
            _ => panic!("compression opt type {t}"),
        }
    }
}

/// The type @src is compressed as into @dst, @src_len and @dst_len how much
/// of each - or none or incompressible. @src a slice: what's compressed is
/// written out, and has to be what's checksummed.
fn compress(fs: &Fs, dst: VolatileSlice<'_>, dst_len: &mut usize, src: &[u8], src_len: &mut usize,
            compression_opt: u8, write_pos: c::bpos) -> ty {
    let block = fs.block_bytes() as usize;

    // If it's only one block, don't bother trying to compress:
    if *src_len <= block {
        return ty::BCH_COMPRESSION_TYPE_incompressible;
    }

    let bits = bch_compression_opt::bits(compression_opt);
    let (t, level) = (bits.type_() as usize, bits.level() as u32);
    let Some(compression_type) = bch_compression_opt::type_of(t as u8) else {
        panic!("compressing with opt {compression_opt:#x}");
    };

    if unlikely(!fs.compress.workspace[t].initialized()) {
        let fix = fsck_err!(fs, id::compression_opt_not_marked_in_sb,
                            "compression opt {} set but not marked in superblock", opts(t as u32));
        // not to fix, fsck_err() failing, or no memory for the pools: don't compress
        if !matches!(fix, Ok(true)) || check_set_has_compressed_data(fs, t as u8).is_err() {
            return ty::BCH_COMPRESSION_TYPE_none;
        }
    }

    /*
     * Try for what this compression actually needs before falling back to
     * the pool. The pool element is sized for the worst case, so taking one
     * for an ordinary write costs far more memory than the write needs -
     * compression=zstd:3 was being handed a level-22 workspace. The pool is
     * the reserve that guarantees forward progress when memory is tight,
     * which is the only reason it has to exist at all.
     *
     * GFP_NOWAIT because we're in the write path: if this can't be had
     * cheaply, the reserve is right there.
     */
    let ws_size = compress_workspace_size(fs, t, level);
    let (mut owned, mut pooled) = (None, None);
    let ws: &mut [MaybeUninit<u8>] = match KVVec::with_capacity(ws_size, GFP_NOWAIT | __GFP_NOWARN) {
        Ok(v) => &mut owned.insert(v).spare_capacity_mut()[..ws_size],
        Err(_) => pooled.insert(fs.compress.workspace[t].alloc()).as_uninit_mut(),
    };

    /*
     * XXX: this algorithm sucks when the compression code doesn't tell us
     * how much would fit, like LZ4 does:
     */
    let fit = loop {
        if *src_len <= block {
            break false;
        }

        let ret = attempt_compress(fs, ws, dst.sub(0..*dst_len), &src[..*src_len], t, level);
        if ret > 0 {
            *dst_len = ret as usize;
            break true;
        }

        // Didn't fit: should we retry with a smaller amount?
        if *src_len <= *dst_len {
            break false;
        }

        // If ret is negative, it's a hint as to how much data would fit
        assert!(ret.unsigned_abs() < *src_len, "compressor's hint {ret}, of {src_len} bytes");
        *src_len = if ret < 0 { ret.unsigned_abs() } else { *src_len - (*src_len - *dst_len) / 2 };
        *src_len -= *src_len % block;
    };

    // Didn't fit, or didn't get smaller:
    let padded = dst_len.next_multiple_of(block);
    if !fit || padded >= *src_len {
        return ty::BCH_COMPRESSION_TYPE_incompressible;
    }

    dst.sub(*dst_len..padded).fill(0);
    *dst_len = padded;

    if unlikely(verify_compress.get()) && !verify(fs, dst.sub(0..*dst_len), &src[..*src_len], compression_type, write_pos) {
        return ty::BCH_COMPRESSION_TYPE_incompressible;
    }
    assert!(*dst_len % block == 0 && *src_len % block == 0,
            "compressed {src_len} bytes to {dst_len}, not whole {block} byte blocks");
    compression_type
}

fn verify(fs: &Fs, dst: VolatileSlice<'_>, src: &[u8], t: ty, pos: c::bpos) -> bool {
    let src_len = src.len();
    let crc = c::bch_extent_crc_unpacked {
        compressed_size:   (dst.len() >> 9) as u32,
        uncompressed_size: (src_len >> 9) as u32,
        compression_type:  t.0 as u8,
        ..Default::default()
    };
    let check = bounce_alloc(fs, src_len, WRITE);
    buf_uncompress(fs, check.data(), dst, &crc).expect("decompressing what we just compressed");
    // SAFETY: ours, and decompressed in full
    if unsafe { check.data().assume_stable() } == src {
        return true;
    }

    let mut msg = LogMsg::new(fs);
    write!(msg, "Decompressing compressed data did not produce the same result ({})", t);
    let trans = btree_trans!(fs);
    let _ = lockrestart_do(&trans, |t| crate::init::error::inum_offset_err_msg(t, &mut msg, 0, pos));
    drop(trans);
    writeln!(msg, " len {src_len}");
    let suppress = count_fsck_err(fs, id::compression_error, &mut msg);
    msg.set_suppressed(suppress);
    false
}

/// # Safety
/// @dst and @src are bios, ours.
#[c_export]
pub unsafe fn bch2_bio_compress(c: &Opaque<c::bch_fs>, dst: *mut c::bio, dst_len: &mut usize,
                                src: *mut c::bio, src_len: &mut usize, compression_opt: c_uint,
                                write_pos: c::bpos, bounce_source: bool) -> c_uint {
    let fs = Fs::from_c(c);

    // SAFETY: the caller's - their sizes ours to clamp, and put back
    let (dst_size, src_size) = unsafe { ((*dst).bi_iter.bi_size, (*src).bi_iter.bi_size) };
    // Don't consume more than encoded_extent_max from @src, or make more than it:
    let consume_src = src_size.min(extent_max(fs) as u32);
    let consume_dst = dst_size.min(consume_src);
    // SAFETY: as above
    unsafe { ((*dst).bi_iter.bi_size, (*src).bi_iter.bi_size) = (consume_dst, consume_src) };
    (*dst_len, *src_len) = (consume_dst as usize, consume_src as usize);

    let t = {
        // SAFETY: the caller's
        let (dst, src) = unsafe { (Bio::from_raw(dst).range(), Bio::from_raw(src).range()) };
        let dst_data = map_or_bounce(fs, dst, WRITE);
        let src_data = if bounce_source { bio_bounce(fs, src, READ) } else { map_or_bounce(fs, src, READ) };

        // SAFETY: a copy, ours - or the bio's own pages, which C, not asking for
        // a copy, says are stable: BCH_WRITE_pages_stable
        let src_slice = unsafe { src_data.data().assume_stable() };
        let t = compress(fs, dst_data.data(), dst_len, src_slice, src_len, compression_opt as u8, write_pos);
        if t != ty::BCH_COMPRESSION_TYPE_none && t != ty::BCH_COMPRESSION_TYPE_incompressible {
            if !dst_data.mapped() {
                // past what's compressed, uninitialized and never submitted
                dst.copy_from_volatile(dst_data.data());
            }
            assert!(*dst_len != 0 && *dst_len <= consume_dst as usize, "compressed to {dst_len} of {consume_dst} bytes");
            assert!(*src_len != 0 && *src_len <= consume_src as usize, "compressed {src_len} of {consume_src} bytes");
        }
        t
    };

    // SAFETY: as above, the references gone
    unsafe { ((*dst).bi_iter.bi_size, (*src).bi_iter.bi_size) = (dst_size, src_size) };
    t.0
}

// ── Setup ────────────────────────────────────────────────────────────────

pub(crate) fn check_set_has_compressed_data(fs: &Fs, compression_opt: u8) -> Result<(), BchError> {
    let t = bch_compression_opt::bits(compression_opt).type_() as usize;
    assert!(t < OPT_NR, "compression opt {compression_opt:#x}");
    if t == 0 {
        return Ok(());
    }

    let f = opt_feature(t);
    if fs.sb.features.get() & f == f {
        return Ok(());
    }
    let mut sb = fs.sb_lock();
    if fs.sb.features.get() & f == f {
        return Ok(());
    }

    init_features(fs, fs.sb.features.get() | f)?;
    sb.sb_mut().features[0] |= f;
    fs.write_super(&mut sb);
    Ok(())
}

/// The pools @features' types need, set up if they aren't.
fn init_features(fs: &Fs, features: u64) -> Result<(), BchError> {
    // SAFETY: pure functions
    let (zstd, deflate, inflate, zstd_d) = unsafe {
        let params = c::zstd_get_params(c::zstd_max_clevel(), extent_max(fs) as u64);
        (c::zstd_cctx_workspace_bound(&params.cParams),
         c::zlib_deflate_workspacesize(c::MAX_WBITS as c_int, c::DEF_MEM_LEVEL as c_int) as usize,
         c::zlib_inflate_workspacesize() as usize,
         c::zstd_dctx_workspace_bound())
    };
    let lz4 = (c::LZ4_MEM_COMPRESS as usize).max(c::LZ4HC_MEM_COMPRESS as usize);
    let types = [
        (opts::BCH_COMPRESSION_OPT_lz4, lz4, 0),
        (opts::BCH_COMPRESSION_OPT_gzip, deflate, inflate),
        (opts::BCH_COMPRESSION_OPT_zstd, zstd, zstd_d),
    ].map(|(t, w, d)| (t.0 as usize, w, d));
    let has = |t| features & opt_feature(t) != 0;

    if !types.iter().any(|&(t, ..)| has(t)) {
        return Ok(());
    }

    let pool = |p: &Mempool, size, err: e| p.init_kvmalloc(1, size).map_err(|_| fs.err(err));
    let pools = &fs.compress;
    pool(&pools.bounce[READ], extent_max(fs), e::BCH_ERR_ENOMEM_compression_bounce_read_init)?;
    pool(&pools.bounce[WRITE], extent_max(fs), e::BCH_ERR_ENOMEM_compression_bounce_write_init)?;

    for (t, compress, decompress) in types.into_iter().filter(|&(t, ..)| has(t)) {
        pool(&pools.workspace[t], compress, e::BCH_ERR_ENOMEM_compression_workspace_init)?;
        if decompress != 0 {
            pool(&pools.decompress[t], decompress, e::BCH_ERR_ENOMEM_compression_workspace_init)?;
        }
    }
    Ok(())
}

#[c_export]
pub fn bch2_fs_compress_init(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    let opt = |v: u8| opt_feature(bch_compression_opt::bits(v).type_().into());
    let f = fs.sb.features.get() | opt(fs.opts().compression.get()) | opt(fs.opts().background_compression.get());
    ret_to_c(init_features(fs, f))
}

// ── Options ──────────────────────────────────────────────────────────────

/// compression=, background_compression=: a type, and a level - "zstd:3".
pub const OPT: crate::opts::OptFn = crate::opts::OptFn {
    parse:    |_, val, err| opt_parse(val, err),
    to_text:  |out, _, _, v| opt_to_text(out, v),
    validate: Some(opt_validate),
};

fn opt_parse(val: Option<&[u8]>, err: Option<&mut Printbuf>) -> Result<u64, BchError> {
    let Some(s) = val else {
        if let Some(err) = err {
            writeln!(err, "compression type required");
        }
        return Err(e::BCH_ERR_EINVAL_opt_parse_str_required.into());
    };
    let colon = s.iter().position(|&b| b == b':');

    let names: &[&str] = crate::names_by_value!(opts);
    let Some(t) = names.iter().position(|n| n.as_bytes() == &s[..colon.unwrap_or(s.len())]) else {
        if let Some(err) = err {
            writeln!(err, "invalid compression type");
        }
        return Err(c::EINVAL.into());
    };

    let mut level = 0;
    if let Some(i) = colon {
        let l = core::str::from_utf8(&s[i + 1..])
            .map_err(|_| BchError::from(c::EINVAL))
            .and_then(crate::util::sysfs::parse_int::<u32>)
            .and_then(|l| if l > 15 || (t == 0 && l != 0) { Err(c::EINVAL.into()) } else { Ok(l) });
        level = match l {
            Ok(l)    => l,
            Err(ret) => {
                if let Some(err) = err {
                    writeln!(err, "invalid compression level");
                }
                return Err(ret);
            }
        };
    }
    Ok(bch_compression_opt::value(t as u8, level as u8).into())
}

pub fn opt_to_text(out: &mut Printbuf, v: u64) {
    let bits = bch_compression_opt::bits(v as u8);
    opts(bits.type_().into()).to_text(out);
    if bits.level() != 0 {
        write!(out, ":{}", bits.level());
    }
}

fn opt_validate(v: u64, err: Option<&mut Printbuf>) -> Result<(), BchError> {
    if !bch_compression_opt::valid(v as u8) {
        if let Some(err) = err {
            write!(err, "invalid compression opt {v}");
        }
        return Err(e::BCH_ERR_invalid_sb_opt_compression.into());
    }
    Ok(())
}

