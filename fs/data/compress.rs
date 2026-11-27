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
//! type is an opt's, or incompressible, or lz4_old. A codec reads an extent's
//! data as one buffer, Src, and writes it to one, Dst: the bio's own memory if
//! it's virtually contiguous or can be vmapped, else a bounce buffer from a
//! pool - a copy, to read; to write, unwritten until the codec has. A bio's
//! memory is only ever a VolatileSlice here: it can change under us
//! (util/volatile.rs).
//!
//! Buffers and workspaces come from the allocator when it has them cheaply,
//! else from mempools, made as each type is enabled and kept: a mempool can't
//! grow. A type's compression pool being there is its being marked in the
//! superblock.

#![allow(non_camel_case_types)]

use crate::c;
use crate::data::extents::ExtentCrcUnpacked;
use crate::format::{CompressionOpt as opts, Bpos, CompressionType, SbFeatures, CompressionOpt_arms};
use crate::errcode::{Errcode as e, BchError, ZSTD_ERRS};
use crate::{bch_err, btree::iter::lockrestart_do, btree_trans, fs::{Fs, SbLockGuard}, fsck_err};
use crate::init::error::{count_fsck_err, id};
use crate::sb::errors::decompress_sb_err;
use crate::os::kernel::{unlikely, READ, WRITE};
use crate::os::bio::{BioRange, Mapped};
use crate::util::{alloc::Box,
                  late::{Late, SbLock}, log::LogMsg, mempool::Mempool,
                  volatile::{Out, VolatileSlice}, Printbuf, ToText};
use crate::os::compress::{lz4, zlib, zstd};
use core::mem::MaybeUninit;
use cstruct_macros::bitfield;

/// A compression option's value: a type, and a level - 0 for the type's
/// default.
#[bitfield(u8)]
pub struct bch_compression_opt {
    type_: u4,
    level: u4,
}

impl bch_compression_opt {
    /// The compression type opt type @t writes, if it's one.
    pub fn type_of(t: u8) -> Option<CompressionType> {
        macro_rules! map {
            ($(($t:ident, $n:literal)),* $(,)?) => {
                match t {
                    $($n => Some(CompressionType::$t),)*
                    _ => None,
                }
            };
        }
        CompressionOpt_arms!(map)
    }

    /// A type there is, and a level only with one.
    pub fn valid(v: u8) -> bool {
        let bits = Self::from_bits(v);
        Self::type_of(bits.type_()).is_some() && !(bits.type_() == 0 && bits.level() != 0)
    }

    /// The compression type @v writes. An unknown type is none - options are
    /// validated, so that's warned about.
    pub fn to_type(v: u8, fs: &Fs) -> CompressionType {
        Self::type_of(Self::from_bits(v).type_()).unwrap_or_else(|| {
            crate::warn_once!(true, fs, "unknown compression opt {v}");
            CompressionType::none
        })
    }
}

crate::module_param! {
    pub(crate) static verify_compress: bool = cfg!(CONFIG_BCACHEFS_DEBUG);
    perm = 0o644, desc = "Decompress data immediately after compressing, and verify the result";
}

const OPT_NR: usize = opts::NR as usize;


/// The opt whose pools decompress type @t.
fn type_to_opt(t: CompressionType) -> usize {
    match t {
        CompressionType::none | CompressionType::incompressible => opts::none,
        CompressionType::lz4_old | CompressionType::lz4 => opts::lz4,
        CompressionType::gzip => opts::gzip,
        CompressionType::zstd => opts::zstd,
        _ => panic!("compression type {}", t.0),
    }.0 as usize
}

/// Opt type @t's superblock feature. None's is lz4's: compression=none
/// still gets lz4's pools at mount.
fn opt_feature(t: usize) -> SbFeatures {
    macro_rules! map {
        ((none, $n0:literal), $(($t:ident, $n:literal)),* $(,)?) => {
            match t {
                $n0 => SbFeatures::lz4,
                $($n => SbFeatures::$t,)*
                _ => panic!("compression opt type {t}"),
            }
        };
    }
    CompressionOpt_arms!(map)
}

fn zstd_errcode(code: u32) -> e {
    macro_rules! map {
        ($(($n:ident, $nr:literal)),* $(,)?) => { ::paste::paste! {
            match code {
                $(c::[<ZSTD_ErrorCode_ZSTD_error_ $n>] => e::[<ZSTD_error_ $n>],)*
                _ => e::ZSTD_error_unknown,
            }
        } };
    }
    ZSTD_ERRS!(map)
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
    bounce:     [Late<Mempool, SbLock>; 2],
    workspace:  [Late<Mempool, SbLock>; OPT_NR],
    decompress: [Late<Mempool, SbLock>; OPT_NR],
}

// ── Bounce buffers ───────────────────────────────────────────────────────

/*
 * Bounce buffer for the encode/decode paths: either a direct mapping of a
 * bio's pages (no copy) or a private allocation the caller can scribble on
 * without touching the bio.
 */
pub(crate) type Bounce<'a, T> = Box<T, &'a Mempool>;

/// What a codec reads: the bio's memory, or a copy of it. 'a is what it
/// borrows: the bio whose memory it maps, or the filesystem whose pool it's
/// from.
pub(crate) enum Src<'a> {
    Mapped(Mapped<'a>),
    Bounced(Bounce<'a, [u8]>),
}

impl Src<'_> {
    /// Ours, or the bio's - which may be written meanwhile.
    pub(crate) fn data(&mut self) -> VolatileSlice<'_> {
        match self {
            Src::Mapped(m) => m.data(),
            Src::Bounced(b) => VolatileSlice::from(&mut **b),
        }
    }
}

/// Where a codec writes: the bio's memory, or a buffer of ours to copy back
/// from, unwritten until the codec says how much it wrote. As Src.
pub(crate) enum Dst<'a> {
    Mapped(Mapped<'a>),
    Bounced(Bounce<'a, [MaybeUninit<u8>]>),
}

impl Dst<'_> {
    pub(crate) fn out(&mut self) -> Out<'_> {
        match self {
            Dst::Mapped(m) => m.data().into(),
            Dst::Bounced(b) => Out::from(&mut **b),
        }
    }
}

pub(crate) fn bounce_alloc(fs: &Fs, size: usize, rw: usize) -> Bounce<'_, [MaybeUninit<u8>]> {
    assert!(size <= extent_max(fs), "bounce buffer of {size} bytes, past encoded_extent_max {}", extent_max(fs));

    /*
     * Not zeroed - the pool's memory as it is - and it needn't be: every
     * bounce is either fully overwritten before it is read, or its unwritten
     * tail is never consumed. By case:
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
     *    compress() zeroes the block-alignment pad; only that much is copied
     *    back to the bio, which is trimmed to it before submission.
     *
     * The one hole is no_data_io, which can hand an unwritten buffer to
     * userspace - but it is root-only and explicitly unsafe, so we ignore
     * it.
     */
    Box::try_new_uninit_slice_in(size, &*fs.compress.bounce[rw])
        .expect("a pool's allocation: it waits for an element")
}

/// @r's data, copied.
pub(crate) fn bio_bounce<'f>(fs: &'f Fs, r: BioRange<'_>) -> Bounce<'f, [u8]> {
    r.copy_into(bounce_alloc(fs, r.len() as usize, READ))
}

/// @r's data, to read: the bio's own memory if it can be, else a copy.
pub(crate) fn map_or_bounce<'a>(fs: &'a Fs, r: BioRange<'a>) -> Src<'a> {
    assert!(r.len() as usize <= extent_max(fs), "mapping {} bytes, past encoded_extent_max {}",
            r.len(), extent_max(fs));

    r.map().map_or_else(|| Src::Bounced(bio_bounce(fs, r)), Src::Mapped)
}

/// Where @r's data is to be written: the bio's own memory if it can be, else a
/// buffer to copy back from.
pub(crate) fn map_or_alloc<'a>(fs: &'a Fs, r: BioRange<'a>) -> Dst<'a> {
    assert!(r.len() as usize <= extent_max(fs), "mapping {} bytes, past encoded_extent_max {}",
            r.len(), extent_max(fs));

    r.map().map_or_else(|| Dst::Bounced(bounce_alloc(fs, r.len() as usize, WRITE)), Dst::Mapped)
}

// ── Decompression ────────────────────────────────────────────────────────

/// @src decompressed in @dst: all of its uncompressed_size, or an error.
pub(crate) fn buf_uncompress<'a>(fs: &Fs, dst: Out<'a>, src: VolatileSlice<'_>, crc: &ExtentCrcUnpacked)
    -> Result<VolatileSlice<'a>, BchError>
{
    let t = crc.compression_type.get();
    let opt = type_to_opt(t);

    /*
     * The compression pool is what tells us the type is marked in the
     * superblock: both pools are set up together, and lz4 needs no
     * decompression workspace at all, so its decompress pool is never
     * initialized and can't stand in for the check.
     */
    if unlikely(!fs.compress.workspace[opt].is_made()) {
        if !fsck_err!(fs, id::compression_type_not_marked_in_sb,
                      "compression type {} set but not marked in superblock", t)? {
            return fs.throw(e::compression_workspace_not_initialized);
        }
        check_set_has_compressed_data(fs, opt as u8)?;
    }

    let (src_len, dst_len) = ((crc.compressed_size as usize) << 9, (crc.uncompressed_size as usize) << 9);
    assert!(src.len() >= src_len && dst.len() >= dst_len && !dst.overlaps(&src),
            "decompressing {src_len} bytes from a buffer of {} into {dst_len} of {}, or overlapping",
            src.len(), dst.len());
    let (src, dst) = (src.sub(0..src_len), dst.sub(0..dst_len));
    // none for lz4, and for gzip in userspace, whose workspace is 0 bytes
    let pool = &fs.compress.decompress[opt];
    let mut ws = pool.is_made().then(|| {
        Box::<[u8], _>::try_new_uninit_slice_in(pool.element_size(), &**pool)
            .expect("a pool's allocation: it waits for an element")
    });
    let ws: &mut [MaybeUninit<u8>] = ws.as_deref_mut().unwrap_or(&mut []);

    Ok(match t {
        CompressionType::lz4_old | CompressionType::lz4 => {
            match lz4::decompress_safe_partial(src, dst, dst_len) {
                Some(d) if d.len() == dst_len => d,
                _ => return fs.throw(if t == CompressionType::lz4 { e::decompress_lz4 }
                                     else { e::decompress_lz4_old }),
            }
        }
        CompressionType::gzip => {
            match zlib::inflate(src, dst, ws, -(c::MAX_WBITS as i32)) {
                None => return fs.throw(e::decompress_gzip),
                Some(d) if d.len() == dst_len => d,
                Some(_) => return fs.throw(e::decompress_gzip_size_mismatch),
            }
        }
        CompressionType::zstd => {
            let mut header = [0u8; 4];
            let real_src_len = match src_len {
                4.. => {
                    src.sub(0..4).copy_to_slice(&mut header);
                    u32::from_le_bytes(header) as usize
                }
                _ => usize::MAX,
            };
            if real_src_len > src_len.saturating_sub(4) {
                return fs.throw(e::decompress_zstd_src_len_bad);
            }

            let Some(mut dctx) = zstd::Dctx::new(ws) else {
                return fs.throw(e::compression_workspace_not_initialized);
            };
            match dctx.decompress(dst, src.sub(4..4 + real_src_len)) {
                Err(code) => return fs.throw(zstd_errcode(code)),
                Ok(d) if d.len() != dst_len => {
                    bch_err!(fs, "zstd decompress error: expected {dst_len} bytes, got {}", d.len());
                    return fs.throw(e::decompress_zstd_size_mismatch);
                }
                Ok(d) => d,
            }
        }
        _ => panic!("decompressing compression type {}", t.0),
    })
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
pub(crate) fn decompress_err(fs: &Fs, ret: Result<(), BchError>) -> Result<(), BchError> {
    if let Err(err) = ret {
        if fs.opts().no_data_io.get() == 0 {
            fs.sb_error_count(decompress_sb_err(err.raw()));
        }
    }
    ret
}

/// @src, the compressed extent, decompressed into @dst.
pub fn bio_uncompress(fs: &Fs, src: BioRange<'_>, dst: BioRange<'_>, crc: ExtentCrcUnpacked)
    -> Result<(), BchError>
{
    let dst_len = (crc.uncompressed_size as usize) << 9;

    assert!(dst.len() as usize + ((crc.offset as usize) << 9) <= dst_len,
            "decompressing {} bytes at sector {} of an extent of {dst_len}", dst.len(), crc.offset);
    assert_eq!(src.len(), crc.compressed_size << 9, "decompressing a bio that isn't the compressed extent");

    if (crc.uncompressed_size.max(crc.compressed_size) as usize) << 9 > extent_max(fs) {
        return decompress_err(fs, fs.throw(e::decompress_exceeded_max_encoded_extent));
    }

    let mut dst_buf = if dst.len() as usize == dst_len {
        map_or_alloc(fs, dst)
    } else {
        Dst::Bounced(bounce_alloc(fs, dst_len, WRITE))
    };
    let mut src_buf = map_or_bounce(fs, src);

    let bounced = matches!(dst_buf, Dst::Bounced(_));
    let ret = buf_uncompress(fs, dst_buf.out(), src_buf.data(), &crc).map(|d| {
        if bounced {
            dst.copy_from_volatile(d.sub(((crc.offset as usize) << 9)..dst_len));
        }
    });
    decompress_err(fs, ret)
}

// ── Compression ──────────────────────────────────────────────────────────

/// Ours go to 15, zstd's to 22.
fn zstd_params(fs: &Fs, level: u32) -> zstd::Params {
    zstd::get_params(((level * 3) / 2).min(zstd::max_clevel() as u32) as i32, extent_max(fs) as u64)
}

/*
 * What this particular compression needs, which is generally much less than
 * the mempool element holds: the pool is sized for the worst case any caller
 * could ask for - highest level, and for lz4 the high-compression variant.
 */
fn compress_workspace_size(fs: &Fs, t: usize, level: u32) -> usize {
    match opts(t as u32) {
        opts::lz4 if level < lz4::HC_MIN_CLEVEL => lz4::MEM_COMPRESS,
        opts::lz4 => lz4::HC_MEM_COMPRESS,
        opts::gzip => zlib::deflate_workspace_size(c::MAX_WBITS as i32, zlib::DEF_MEM_LEVEL),
        opts::zstd => zstd::cctx_workspace_bound(&zstd_params(fs, level)),
        _ => panic!("compression opt type {t}"),
    }
}

/// @src, compressed; if it didn't fit, negative - how much of @src would, if
/// the compressor can say - or 0.
fn attempt_compress<'a>(fs: &Fs, ws: &mut [MaybeUninit<u8>], dst: Out<'a>, src: &[u8], t: usize, level: u32)
    -> Result<VolatileSlice<'a>, isize>
{
    /*
     * LZ4HC uses LZ4_wildCopy(), which may overwrite up to 7 bytes past
     * its requested copy end. Keep that inside our bounce buffer even when
     * the compressed output ends exactly at the supplied limit.
     */
    const LZ4HC_OUTPUT_SLOP: usize = 7;

    let (src_len, dst_len) = (src.len(), dst.len());
    assert!(src_len % 512 == 0 && dst_len % 512 == 0, "compressing {src_len} bytes into {dst_len}, not sectors");

    match opts(t as u32) {
        opts::lz4 if level < lz4::HC_MIN_CLEVEL => {
            let (compressed, len) = lz4::compress_dest_size(src, dst, ws);
            if len < src_len { Err(-(len as isize)) } else { compressed.ok_or(0) }
        }
        opts::lz4 => {
            if dst_len <= LZ4HC_OUTPUT_SLOP {
                return Err(-1);
            }
            lz4::compress_hc(src, dst.sub(0..dst_len - LZ4HC_OUTPUT_SLOP), level, ws).ok_or(-1)
        }
        opts::gzip => {
            let level = match level {
                0 => c::Z_DEFAULT_COMPRESSION as i32,
                l => (l as i32).clamp(c::Z_BEST_SPEED as i32, c::Z_BEST_COMPRESSION as i32),
            };
            zlib::deflate(src, dst, ws, level, -(c::MAX_WBITS as i32), zlib::DEF_MEM_LEVEL,
                          c::Z_DEFAULT_STRATEGY as i32)
                .ok_or(0)
        }
        opts::zstd => {
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
            let mut cctx = zstd::Cctx::new(ws).ok_or(0isize)?;
            let body = cctx.compress(dst.sub(4..dst_len - 7), src, &zstd_params(fs, level)).map_err(|_| 0isize)?;
            let header = dst.sub(0..4).copy_from_slice(&(body.len() as u32).to_le_bytes());
            Ok(dst.joined(header, body))
        }
        _ => panic!("compression opt type {t}"),
    }
}

/// The type @src is compressed as into @dst, and what it was compressed to -
/// @src_len how much of it - or none or incompressible. @src a slice: what's
/// compressed is written out, and has to be what's checksummed.
fn compress<'a>(fs: &Fs, dst: Out<'a>, src: &[u8], src_len: &mut usize,
                compression_opt: u8, write_pos: Bpos) -> (CompressionType, Option<VolatileSlice<'a>>) {
    let block = fs.block_bytes() as usize;

    // If it's only one block, don't bother trying to compress:
    if *src_len <= block {
        return (CompressionType::incompressible, None);
    }

    let bits = bch_compression_opt::from_bits(compression_opt);
    let (t, level) = (bits.type_() as usize, bits.level() as u32);
    let Some(compression_type) = bch_compression_opt::type_of(t as u8) else {
        panic!("compressing with opt {compression_opt:#x}");
    };

    if unlikely(!fs.compress.workspace[t].is_made()) {
        let fix = fsck_err!(fs, id::compression_opt_not_marked_in_sb,
                            "compression opt {} set but not marked in superblock", opts(t as u32));
        // not to fix, fsck_err() failing, or no memory for the pools: don't compress
        if !matches!(fix, Ok(true)) || check_set_has_compressed_data(fs, t as u8).is_err() {
            return (CompressionType::none, None);
        }
    }

    /*
     * What this compression actually needs, which the pool tries for before
     * its element. The element is sized for the worst case, so taking one
     * for an ordinary write costs far more memory than the write needs -
     * compression=zstd:3 was being handed a level-22 workspace. The pool is
     * the reserve that guarantees forward progress when memory is tight,
     * which is the only reason it has to exist at all - and without reclaim
     * first, as we're in the write path: if this can't be had cheaply, the
     * reserve is right there.
     */
    let ws_size = compress_workspace_size(fs, t, level);
    let mut ws = Box::try_new_uninit_slice_in(ws_size, &*fs.compress.workspace[t])
        .expect("a pool's allocation: it waits for an element");
    let ws: &mut [MaybeUninit<u8>] = &mut ws;

    /*
     * XXX: this algorithm sucks when the compression code doesn't tell us
     * how much would fit, like LZ4 does:
     */
    let compressed = loop {
        if *src_len <= block {
            break None;
        }

        let ret = match attempt_compress(fs, ws, dst, &src[..*src_len], t, level) {
            Ok(compressed) => break Some(compressed),
            Err(ret) => ret,
        };

        // Didn't fit: should we retry with a smaller amount?
        if *src_len <= dst.len() {
            break None;
        }

        // If ret is negative, it's a hint as to how much data would fit
        assert!(ret.unsigned_abs() < *src_len, "compressor's hint {ret}, of {src_len} bytes");
        *src_len = if ret < 0 { ret.unsigned_abs() } else { *src_len - (*src_len - dst.len()) / 2 };
        *src_len -= *src_len % block;
    };

    // Didn't fit, or didn't get smaller:
    let Some(compressed) = compressed else {
        return (CompressionType::incompressible, None);
    };
    let padded = compressed.len().next_multiple_of(block);
    if padded >= *src_len {
        return (CompressionType::incompressible, None);
    }

    let compressed = dst.joined(compressed, dst.sub(compressed.len()..padded).fill(0));

    if unlikely(verify_compress.get()) && !verify(fs, compressed, &src[..*src_len], compression_type, write_pos) {
        return (CompressionType::incompressible, None);
    }
    assert!(compressed.len() % block == 0 && *src_len % block == 0,
            "compressed {src_len} bytes to {}, not whole {block} byte blocks", compressed.len());
    (compression_type, Some(compressed))
}

fn verify(fs: &Fs, dst: VolatileSlice<'_>, src: &[u8], t: CompressionType, pos: Bpos) -> bool {
    let src_len = src.len();
    let crc = ExtentCrcUnpacked {
        compressed_size:   (dst.len() >> 9) as u32,
        uncompressed_size: (src_len >> 9) as u32,
        compression_type:  t.into(),
        ..Default::default()
    };
    let mut check = bounce_alloc(fs, src_len, WRITE);
    if buf_uncompress(fs, Out::from(&mut *check), dst, &crc).expect("decompressing what we just compressed")
        .eq_slice(src) {
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

/// Not inlined: in __write() its locals were 700 bytes of the frame, on every
/// uncompressed write too.
#[inline(never)]
pub(crate) fn bio_compress(fs: &Fs, dst: BioRange<'_>, dst_len: &mut usize,
                           src: BioRange<'_>, src_len: &mut usize, compression_opt: u32,
                           write_pos: Bpos, bounce_source: bool) -> CompressionType {
    // Don't consume more than encoded_extent_max from @src, or make more than it:
    let consume_src = src.len().min(extent_max(fs) as u32);
    let consume_dst = dst.len().min(consume_src);
    let (dst, src) = (dst.take(consume_dst), src.take(consume_src));
    (*dst_len, *src_len) = (consume_dst as usize, consume_src as usize);

    let mut dst_data = map_or_alloc(fs, dst);
    let mut src_data = if bounce_source { Src::Bounced(bio_bounce(fs, src)) } else { map_or_bounce(fs, src) };

    let src_slice: &[u8] = match &mut src_data {
        Src::Bounced(b) => b,
        // SAFETY: the bio's own pages, which C, not asking for a copy, says are
        // stable: BCH_WRITE_pages_stable
        Src::Mapped(m) => unsafe { m.data().assume_stable() },
    };
    let bounced = matches!(dst_data, Dst::Bounced(_));
    let (t, compressed) = compress(fs, dst_data.out(), src_slice, src_len, compression_opt as u8, write_pos);
    if let Some(compressed) = compressed {
        if bounced {
            dst.copy_from_volatile(compressed);
        }
        *dst_len = compressed.len();
        assert!(*dst_len != 0 && *dst_len <= consume_dst as usize, "compressed to {dst_len} of {consume_dst} bytes");
        assert!(*src_len != 0 && *src_len <= consume_src as usize, "compressed {src_len} of {consume_src} bytes");
    }
    t
}

// ── Setup ────────────────────────────────────────────────────────────────

pub(crate) fn check_set_has_compressed_data(fs: &Fs, compression_opt: u8) -> Result<(), BchError> {
    let t = bch_compression_opt::from_bits(compression_opt).type_() as usize;
    assert!(t < OPT_NR, "compression opt {compression_opt:#x}");
    if t == 0 {
        return Ok(());
    }

    let f = opt_feature(t);
    if fs.sb.features.get().contains(f) {
        return Ok(());
    }
    let mut sb = fs.sb_lock();
    if fs.sb.features.get().contains(f) {
        return Ok(());
    }

    init_features(fs, &sb, fs.sb.features.get() | f)?;
    sb.sb_mut().features.update(|x| x | f);
    fs.write_super(&mut sb);
    Ok(())
}

/// The pools @features' types need, set up if they aren't.
fn init_features(fs: &Fs, sb: &SbLockGuard<'_>, features: SbFeatures) -> Result<(), BchError> {
    let types = [
        (opts::lz4, lz4::MEM_COMPRESS.max(lz4::HC_MEM_COMPRESS), 0),
        (opts::gzip, zlib::deflate_workspace_size(c::MAX_WBITS as i32, zlib::DEF_MEM_LEVEL),
         zlib::inflate_workspace_size()),
        (opts::zstd, zstd::cctx_workspace_bound(&zstd::get_params(zstd::max_clevel(), extent_max(fs) as u64)),
         zstd::dctx_workspace_bound()),
    ].map(|(t, w, d)| (t.0 as usize, w, d));
    let has = |t| features.contains(opt_feature(t));

    if !types.iter().any(|&(t, ..)| has(t)) {
        return Ok(());
    }

    let pool = |p: &Late<Mempool, SbLock>, size, err: e| p.init(sb, || Mempool::new(1, size)).map_err(|_| fs.err(err));
    let pools = &fs.compress;
    pool(&pools.bounce[READ], extent_max(fs), e::ENOMEM_compression_bounce_read_init)?;
    pool(&pools.bounce[WRITE], extent_max(fs), e::ENOMEM_compression_bounce_write_init)?;

    for (t, compress, decompress) in types.into_iter().filter(|&(t, ..)| has(t)) {
        pool(&pools.workspace[t], compress, e::ENOMEM_compression_workspace_init)?;
        if decompress != 0 {
            pool(&pools.decompress[t], decompress, e::ENOMEM_compression_workspace_init)?;
        }
    }
    Ok(())
}

pub fn compress_init(fs: &Fs) -> Result<(), BchError> {
    let opt = |v: u8| opt_feature(bch_compression_opt::from_bits(v).type_().into());
    let f = fs.sb.features.get() | opt(fs.opts().compression.get()) | opt(fs.opts().background_compression.get());
    init_features(fs, &fs.sb_lock(), f)
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
        return Err(e::EINVAL_opt_parse_str_required.into());
    };
    let colon = s.iter().position(|&b| b == b':');

    let names: &[&str] = crate::names_by_value!(opts);
    let Some(t) = names.iter().position(|n| n.as_bytes() == &s[..colon.unwrap_or(s.len())]) else {
        if let Some(err) = err {
            writeln!(err, "invalid compression type");
        }
        return Err(e::EINVAL_compression_type.into());
    };

    let mut level = 0;
    if let Some(i) = colon {
        let l = core::str::from_utf8(&s[i + 1..])
            .map_err(|_| BchError::from(e::EINVAL_compression_level_utf8))
            .and_then(crate::os::sysfs::parse_int::<u32>)
            .and_then(|l| if l > 15 || (t == 0 && l != 0) { Err(e::EINVAL_compression_level.into()) } else { Ok(l) });
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
    Ok(bch_compression_opt::new().with_type_(t as u8).with_level(level as u8).into_bits().into())
}

pub fn opt_to_text(out: &mut Printbuf, v: u64) {
    let bits = bch_compression_opt::from_bits(v as u8);
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
        return Err(e::invalid_sb_opt_compression.into());
    }
    Ok(())
}

