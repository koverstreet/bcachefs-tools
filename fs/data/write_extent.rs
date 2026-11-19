// SPDX-License-Identifier: GPL-2.0

//! A write's data made into extents at its write point: bch2_write_extent(),
//! with the bios and encoding that takes.

use crate::alloc::sector_alloc::LockedWritePoint;
use crate::util::alloc::flags;
use crate::c;
use crate::data::compress::{bio_bounce, bio_compress, bounce_alloc, buf_uncompress, decompress_err, map_or_bounce, Src};
use crate::data::ptrs::{crc_is_compressed, extent_crc_append, extent_nonce};
use crate::data::write::{write_op_error, ExtentBio};
use crate::data::write_replicas::wbio_in_place;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::locking::MemallocFlags;
use crate::os::bio::{pages, Bio, Buffer, HasBio, Pages};
use crate::util::volatile::Out;
use crate::os::kernel::{unlikely, PAGE_SIZE, WRITE};
use c::WriteFlags as w;
use core::ffi::c_void;
use crate::format::{Bversion, BkeyType};

type Op = c::bch_write_op;

fn csum_differs(a: c::bch_csum, b: c::bch_csum) -> bool {
    ((a.lo.get() ^ b.lo.get()) | (a.hi.get() ^ b.hi.get())) != 0
}

fn init_append_extent(op: &mut Op, wp: &mut LockedWritePoint<'_>, version: Bversion,
                      crc: c::bch_extent_crc_unpacked) {
    /*
     * The key covers the crc's live data, not all of it: an extent that won't
     * decompress is written as is, untrimmed - its crc's offset and
     * uncompressed size still those of the whole extent it was cut from:
     */
    op.pos.offset += u64::from(crc.live_size);

    let (pos, cached) = (op.pos, op.has(w::cached));
    op.insert_keys.push_with(c::BKEY_EXTENT_U64s_MAX as usize, |e| {
        e.k = c::bkey { type_: BkeyType::extent.0 as u8, p: pos, bversion: version,
                               ..c::bkey::new() };
        e.k.size = crc.live_size;

        if crc.csum_type != c::CsumType::none || crc.compression_type != c::CompressionType::none || crc.nonce != 0 {
            extent_crc_append(e, &crc).unwrap_or_else(|err| panic!("an extent's crc past its room: {err}"));
        }

        wp.append_ptrs(e, crc.compressed_size, cached);
    });
}

#[derive(Default)]
struct AllocShort {
    /// Of the pages it wanted: what the bounce pool made up is all the
    /// extent's to have
    pages: bool,
    /// It took from the bounce pool
    pool:  bool,
}

fn write_bio_alloc(fs: &'static Fs, wp: &LockedWritePoint<'_>, src: &Bio, short: &mut AllocShort,
                   buf: *mut c_void) -> ExtentBio {
    let mut output_available = (wp.sectors_free() << 9).min(src.range().len());
    let offset = if buf.is_null() { 0 } else { (buf.addr() & (PAGE_SIZE - 1)) as u32 };

    /*
     * XXX: we'll want to delete this later, there's no reason we can't
     * issue > 2MB bios if we're allocating high order pages
     *
     * But bch2_bio_alloc_pages() BUGS() if we ask it to allocate more pages
     * than fit in the bio, and we're using bio_alloc_bioset() which is
     * limited to BIO_MAX_VECS - and a vmalloc'd @buf not on a page boundary,
     * an ec stripe's, takes a page more than its size:
     */
    output_available = output_available.min(c::BIO_MAX_VECS * PAGE_SIZE as u32 - offset);

    let block_size = u32::from(fs.opts().block_size.get());
    assert!(output_available & (block_size - 1) == 0, "a write bio of {output_available} bytes, not whole blocks");

    /* copy WRITE_SYNC flag */
    let opf = src.opf();

    if !buf.is_null() {
        let buf = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), output_available as usize) };
        let mut b = fs.bio_write.alloc(Buffer::nr_vecs(&buf), opf, wbio_in_place)
            .unwrap_or_else(|e: BchError| panic!("a write's bio for its stripe: its bioset waits, but {e}"));
        Buffer::fill(HasBio::bio_mut(b.as_mut()), buf);
        return ExtentBio::Stripe(b);
    }

    /*
     * We can't use mempool for more than c->sb.encoded_extent_max
     * worth of pages, but we'd like to allocate more if we can:
     *
     * CONFIG_INIT_ON_ALLOC_DEFAULT_ON is frequently on and it forces all
     * memory allocations to be zeroed, as a security hardening measure: not
     * unreasonable for data structures, but very expensive and pointless
     * for data buffers we're just passing through and are about to be
     * overwritten with real data:
     */
    let required = output_available.min(fs.opts().encoded_extent_max.get());
    let (bounce, pages) = {
        let _noio = MemallocFlags::noio();
        let size = output_available as usize;
        fs.bio_write.alloc(pages::nr_vecs(size), opf, wbio_in_place).and_then(|mut b| {
            let pages = Pages::fill(HasBio::bio_mut(b.as_mut()), &fs.bio_bounce, block_size, size, required as usize,
                                    flags::NOIO | flags::SKIP_ZERO)?;
            Ok((b, pages))
        })
    }.unwrap_or_else(|e: BchError| panic!("a write's bounce pages, outside NORECLAIM: {e}"));
    short.pool  = pages.used_pool();
    short.pages = bounce.bio.range().len() < output_available;
    ExtentBio::Bounce(bounce, Some(pages))
}

fn write_rechecksum(fs: &Fs, op: &mut Op, mut new_csum_type: c::CsumType) -> Result<(), BchError> {
    let mut new_crc = c::bch_extent_crc_unpacked::default();

    /* bch2_rechecksum_bio() can't encrypt or decrypt data: */

    if op.crc.csum_type.get().is_encryption() != new_csum_type.is_encryption() {
        new_csum_type = op.crc.csum_type.get();
    }

    fs.rechecksum_bio(&op.wbio.bio, op.version, op.crc, None, Some(&mut new_crc),
                      op.crc.offset.into(), op.crc.live_size, new_csum_type)?;

    op.wbio.bio_mut().advance(u32::from(op.crc.offset) << 9);
    op.wbio.bio_mut().shrink(op.crc.live_size << 9);
    op.crc = new_crc;
    Ok(())
}

/*
 * Decode an encoded extent in @bio in place: decrypt if encrypted, then
 * decompress, and only put the result back once we have all of it. This is the
 * inverse of the compress/encrypt the write path does on the way out, which is
 * why it lives here - the decode is a write-path concern, and compress.c owns
 * the primitives it's built from.
 *
 * @bio and @op->crc are untouched unless this returns 0, so a caller that can't
 * use the plaintext still has the extent exactly as it arrived.
 */
fn write_op_decode(op: &mut Op) -> Result<(), BchError> {
    let fs = op.fs();
    let crc = op.crc;
    let dst_len = (crc.uncompressed_size as usize) << 9;

    /* bio must own its pages: */
    assert!(op.wbio.bio.nr_vecs() != 0, "decoding an extent in a bio that doesn't own its pages");
    assert!((crc.live_size as usize) << 9 <= pages::bytes(&op.wbio.bio),
            "decoding an extent bigger than its bio");
    assert!(op.wbio.bio.range().len() == crc.compressed_size << 9, "decoding a bio that isn't its extent");

    if crc.uncompressed_size << 9 > fs.opts().encoded_extent_max.get() {
        write_op_error(op, op.pos.offset, format_args!("extent too big to decompress ({} > {})",
                                                       crc.uncompressed_size << 9,
                                                       fs.opts().encoded_extent_max.get()));
        return decompress_err(fs, Err(fs.err(Errcode::decompress_exceeded_max_encoded_extent)));
    }

    let mut dst_buf = bounce_alloc(fs, dst_len, WRITE);

    /*
     * Encrypted data has to be decrypted before it can be decompressed, and
     * @bio has to stay untouched until we know we have something to put
     * back: if the data won't decompress, the caller writes the extent
     * exactly as it found it. So take a private copy rather than letting
     * bch2_bio_map_or_bounce() hand us the bio's own pages, and decrypt that.
     */
    let range = op.wbio.bio.range();
    let mut src_buf = if crc.csum_type.get().is_encryption() {
        let mut b = bio_bounce(fs, range);
        fs.encrypt(crc.csum_type.get(), extent_nonce(op.version, &crc), &mut b[..(crc.compressed_size as usize) << 9])?;
        Src::Bounced(b)
    } else {
        map_or_bounce(fs, range)
    };

    let mut ret = buf_uncompress(fs, Out::from(&mut *dst_buf), src_buf.data(), &crc).map(|_| ());
    drop(src_buf);
    if fs.opts().no_data_io.get() != 0 {
        ret = Ok(());
    }
    if let Err(e) = ret {
        write_op_error(op, op.pos.offset, format_args!("{e}"));
        return decompress_err(fs, Err(e));
    }

    /*
     * XXX: don't have a good way to assert that the bio was allocated with
     * enough space, we depend on move_extent() doing the right thing
     */
    op.wbio.bio_mut().raw_mut().bi_iter.bi_size = crc.live_size << 9;

    let iter = op.wbio.bio.raw().bi_iter;
    unsafe {
        crate::os::bio::memcpy_to_bio(op.wbio.bio_mut().raw_mut(), iter,
                                        dst_buf.as_ptr().add((crc.offset as usize) << 9).cast());
    }

    let crc = &mut op.crc;
    crc.csum_type.set(c::CsumType::none);
    crc.compression_type.set(c::CompressionType::none);
    crc.compressed_size   = crc.live_size;
    crc.uncompressed_size = crc.live_size;
    crc.offset            = 0;
    crc.csum              = c::bch_csum::default();
    Ok(())
}

fn csum_err(op: &Op, csum: c::bch_csum) -> BchError {
    let fs = op.fs();
    let ty = op.crc.csum_type.get();
    let ty_name: &dyn core::fmt::Display = if ty.0 < c::CsumType::NR { &ty } else { &"(unknown)" };
    write_op_error(op, op.pos.offset, format_args!(
        "error verifying existing checksum while moving existing data (memory corruption?)\n  \
         expected {:0x}:{:0x} got {:0x}:{:0x} type {}",
        op.crc.csum.hi.get(), op.crc.csum.lo.get(), csum.hi.get(), csum.lo.get(), ty_name));
    fs.err(Errcode::data_write_csum)
}

#[inline(never)]
fn write_prep_encoded_data(op: &mut Op, wp: &LockedWritePoint<'_>) -> Result<(), BchError> {
    let fs = op.fs();

    assert!(op.wbio.bio.range().len() >> 9 == op.crc.compressed_size, "an encoded write's bio isn't its extent");

    /* Can we just write the entire extent as is? */
    if op.crc.uncompressed_size == op.crc.live_size &&
       op.crc.uncompressed_size <= fs.opts().encoded_extent_max.get() >> 9 &&
       op.crc.compressed_size <= wp.sectors_free() &&
       op.crc.csum_type.get().is_encryption() == op.bits.csum_type().is_encryption() &&
       (op.crc.compression_type == c::bch_compression_opt::to_type(op.bits.compression_opt(), fs) ||
        op.bits.incompressible() != 0) {
        if op.crc.csum_type != op.bits.csum_type() {
            if !crc_is_compressed(&op.crc) {
                write_rechecksum(fs, op, op.bits.csum_type())?;
            } else {
                /*
                 * bch2_rechecksum_bio() works in uncompressed
                 * units and can't split compressed extents -
                 * but changing the checksum type of a whole
                 * encoded extent doesn't need to: recompute
                 * and verify over the encoded payload.
                 *
                 * The nonce doesn't depend on the checksum
                 * type, so readers derive the same nonce to
                 * verify the new checksum, and the payload -
                 * ciphertext, if encrypted - is untouched:
                 * we're never relabelling between encrypted
                 * and unencrypted checksum types, per the
                 * encryption check above:
                 */
                debug_assert!(op.crc.csum_type.get().is_encryption() == op.bits.csum_type().is_encryption());

                /*
                 * Calculate the new checksum first, then
                 * verify: data corrupted in between gets
                 * caught on read, instead of being blessed
                 * with a fresh valid checksum:
                 */
                let nonce = extent_nonce(op.version, &op.crc);
                let new_csum = fs.checksum_range(op.bits.csum_type(), nonce, op.wbio.bio.range());

                let csum = fs.checksum_range(op.crc.csum_type.get(), nonce, op.wbio.bio.range());
                if csum_differs(op.crc.csum, csum) && fs.opts().no_data_io.get() == 0 {
                    return Err(csum_err(op, csum));
                }

                op.crc.csum_type.set(op.bits.csum_type());
                op.crc.csum = new_csum;
            }
        }

        return Err(fs.err(Errcode::write_encoded_as_is));
    }

    /*
     * If the data is compressed and we couldn't write the entire extent as
     * is, we have to decompress it:
     */
    if crc_is_compressed(&op.crc) {
        /* Last point we can still verify checksum: */
        let nonce = extent_nonce(op.version, &op.crc);
        let csum = fs.checksum_range(op.crc.csum_type.get(), nonce, op.wbio.bio.range());
        if csum_differs(op.crc.csum, csum) && fs.opts().no_data_io.get() == 0 {
            return Err(csum_err(op, csum));
        }

        /*
         * Decryption happens inside bch2_write_op_decode(), on
         * its own copy - @bio and @op->crc are left exactly as they are
         * until the decompressed data is ready to go back. So if the
         * data won't decompress there is nothing to undo, and writing
         * the extent as we found it is still on the table:
         */
        if let Err(e) = write_op_decode(op) {
            if !e.matches(Errcode::decompress) {
                return Err(e);
            }

            /*
             * Nothing will ever decompress this. Preserve what we
             * have rather than failing the write: the extent goes
             * out byte-for-byte as it came in, still encrypted,
             * still compressed, @op->crc still describing it.
             *
             * A compressed extent can't be split, so this needs a
             * write point with room for the whole thing. If we
             * haven't got one, say so and let __bch2_write() retire
             * the short buckets and come back with fresh ones. That
             * only ever happens here, so healthy compressed data
             * never pays the fragmentation for it.
             */
            if op.crc.compressed_size > wp.sectors_free() {
                return Err(fs.err(Errcode::data_write_need_fresh_buckets));
            }

            return Err(fs.err(Errcode::write_encoded_as_is));
        }
    }

    /*
     * No longer have compressed data after this point - data might be
     * encrypted:
     */

    /*
     * If the data is checksummed and we're only writing a subset,
     * rechecksum and adjust bio to point to currently live data:
     */
    if op.crc.live_size != op.crc.uncompressed_size || op.crc.csum_type != op.bits.csum_type() {
        write_rechecksum(fs, op, op.bits.csum_type())?;
    }

    /*
     * If we want to compress the data, it has to be decrypted:
     */
    if op.crc.csum_type.get().is_encryption() &&
       (op.bits.compression_opt() != 0 || op.crc.csum_type != op.bits.csum_type()) {
        let nonce = extent_nonce(op.version, &op.crc);
        let csum = fs.checksum_range(op.crc.csum_type.get(), nonce, op.wbio.bio.range());
        if csum_differs(op.crc.csum, csum) && fs.opts().no_data_io.get() == 0 {
            return Err(csum_err(op, csum));
        }

        fs.encrypt_range(nonce, op.wbio.bio.range())?;

        op.crc.csum_type.set(c::CsumType::none);
        op.crc.csum = c::bch_csum::default();
    }

    Ok(())
}

pub(crate) struct Extent {
    pub bio:      ExtentBio,
    /// Of the op's data, after it
    pub more:     bool,
    /// Its memory is - or may be - a mempool's reserve: the write mustn't
    /// ask for more till it's freed
    pub reserved: bool,
}

/// The next of @op's data written out at @wp: its extents' keys appended,
/// and the bio to submit.
pub(crate) fn write_extent(op: &mut Op, wp: &mut LockedWritePoint<'_>) -> Result<Extent, BchError> {
    let fs = op.fs();
    let mut dst = ExtentBio::Op;
    let (mut total_output, mut total_input) = (0u32, 0u32);
    let mut bounce = false;
    let mut short = AllocShort::default();
    /* the bounce pool's, or a bio of bio_write's while its one reserve bio is out */
    let reserved = |dst: &ExtentBio, short: &AllocShort| short.pool || (!dst.is_op() && fs.bio_write.reserve_drawn());

    if op.bits.incompressible() != 0 {
        op.bits.set_compression_opt(0);
    }

    assert!(op.wbio.bio.range().len() >> 9 != 0, "writing an empty bio");

    let ec_buf = wp.ec_buf();

    if unlikely(op.has(w::data_encoded)) {
        match write_prep_encoded_data(op, wp) {
            Err(e) if e.matches(Errcode::write_encoded_as_is) => {
                if !ec_buf.is_null() {
                    dst = write_bio_alloc(fs, wp, &op.wbio.bio, &mut short, ec_buf);
                    dst.own().expect("a stripe's bio").range().copy_from_range(op.wbio.bio.range());
                }
                init_append_extent(op, wp, op.version, op.crc);
                let reserved = reserved(&dst, &short);
                return Ok(Extent { bio: dst, more: false, reserved });
            }
            Err(e) => return Err(e),
            Ok(()) => {}
        }
    }

    if !ec_buf.is_null() ||
       op.bits.compression_opt() != 0 ||
       (op.bits.csum_type() != c::CsumType::none && !op.has(w::pages_stable)) ||
       (op.bits.csum_type().is_encryption() && !op.has(w::pages_owned)) {
        dst = write_bio_alloc(fs, wp, &op.wbio.bio, &mut short, ec_buf);
        bounce = true;
    }

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    let write_corrupt_ratio = crate::data::write::write_corrupt_ratio.get();
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    if !bounce && write_corrupt_ratio != 0 {
        dst = write_bio_alloc(fs, wp, &op.wbio.bio, &mut short, ec_buf);
        bounce = true;
    }
    let saved_iter = dst.bio_ref(&op.wbio).raw().bi_iter;

    let ret = (|| -> Result<(), BchError> {
        loop {
            let mut crc = c::bch_extent_crc_unpacked::default();
            let mut version = op.version;
            let (mut dst_len, mut src_len) = (0u32, 0u32);
            let src_size = op.wbio.bio.range().len();

            assert!(u64::from(src_size) & (fs.block_bytes() - 1) == 0,
                    "a write of {src_size} bytes, not whole blocks");

            if short.pages &&
               dst.len(&op.wbio) < wp.sectors_free() << 9 &&
               dst.len(&op.wbio) < fs.opts().encoded_extent_max.get() {
                return Ok(());
            }

            assert!(!(op.bits.compression_opt() != 0 && op.has(w::data_encoded) &&
                      op.crc.csum_type.get().is_encryption()), "compressing data still encrypted");
            assert!(!(op.bits.compression_opt() != 0 && !bounce), "compressing without bouncing");

            crc.compression_type.set(if op.bits.incompressible() != 0 {
                c::CompressionType::incompressible
            } else if op.bits.compression_opt() != 0 {
                let (mut d, mut s) = (0usize, 0usize);
                let dst_r = dst.own().expect("compressing into a bio of its own").range();
                let t = bio_compress(fs, dst_r, &mut d, op.wbio.bio.range(), &mut s, op.bits.compression_opt().into(),
                                     op.pos, !op.has(w::pages_stable));
                (dst_len, src_len) = (d as u32, s as u32);
                t
            } else {
                c::CompressionType::none
            });

            if !crc_is_compressed(&crc) {
                dst_len = dst.len(&op.wbio).min(src_size);
                dst_len = dst_len.min(wp.sectors_free() << 9);

                if op.bits.csum_type() != c::CsumType::none {
                    dst_len = dst_len.min(fs.opts().encoded_extent_max.get());
                }

                if bounce {
                    dst.own().expect("bounced into a bio of its own").range().take(dst_len)
                        .copy_from_range(op.wbio.bio.range());
                }

                src_len = dst_len;
            }

            assert!(src_len != 0 && dst_len != 0, "writing an extent of no data");

            if op.bits.csum_type().is_encryption() {
                if version == Bversion::default() {
                    version.lo = (fs.key_version.fetch_add(1, core::sync::atomic::Ordering::SeqCst) + 1) as u64;
                } else {
                    crc.nonce = op.nonce;
                    op.nonce += (src_len >> 9) as u16;
                }
            }

            if op.has(w::data_encoded) &&
               !crc_is_compressed(&crc) &&
               op.crc.csum_type.get().is_encryption() == op.bits.csum_type().is_encryption() {
                let compression_type = crc.compression_type;
                let nonce = crc.nonce;
                /*
                 * Note: when we're using rechecksum(), we need to be
                 * checksumming @src because it has all the data our
                 * existing checksum covers - if we bounced (because we
                 * were trying to compress), @dst will only have the
                 * part of the data the new checksum will cover.
                 *
                 * But normally we want to be checksumming post bounce,
                 * because part of the reason for bouncing is so the
                 * data can't be modified (by userspace) while it's in
                 * flight.
                 */
                let old = op.crc;
                let csum_type = op.bits.csum_type();
                fs.rechecksum_bio(&op.wbio.bio, version, old,
                                  Some(&mut crc), Some(&mut op.crc),
                                  src_len >> 9,
                                  (src_size >> 9) - (src_len >> 9),
                                  csum_type)?;
                /*
                 * rchecksum_bio sets compression_type on crc from op->crc,
                 * this isn't always correct as sometimes we're changing
                 * an extent from uncompressed to incompressible.
                 */
                crc.compression_type = compression_type;
                crc.nonce = nonce;
            } else {
                if op.has(w::data_encoded) {
                    let old = op.crc;
                    fs.rechecksum_bio(&op.wbio.bio, version, old,
                                      None, Some(&mut op.crc),
                                      src_len >> 9,
                                      (src_size >> 9) - (src_len >> 9),
                                      old.csum_type.get())?;
                }

                crc.compressed_size   = dst_len >> 9;
                crc.uncompressed_size = src_len >> 9;
                crc.live_size         = src_len >> 9;

                let extent = dst.bio_ref(&op.wbio).range().take(dst_len);
                let csum_type = op.bits.csum_type();
                if csum_type.is_encryption() {
                    fs.encrypt_range(extent_nonce(version, &crc), extent)?;
                }

                crc.csum = fs.checksum_range(csum_type, extent_nonce(version, &crc), extent);
                crc.csum_type.set(csum_type);
            }

            init_append_extent(op, wp, version, crc);

            #[cfg(CONFIG_BCACHEFS_DEBUG)]
            if write_corrupt_ratio != 0 && crate::os::kernel::random_u64_below(write_corrupt_ratio.into()) == 0 {
                dst.bio_ref(&op.wbio).range().take(dst_len).corrupt();
            }

            if !dst.is_op() {
                dst.bio(&mut op.wbio).advance(dst_len);
            }
            op.wbio.bio_mut().advance(src_len);
            total_output += dst_len;
            total_input  += src_len;

            if !(dst.len(&op.wbio) != 0 &&
                 op.wbio.bio.range().len() != 0 &&
                 wp.sectors_free() != 0 &&
                 op.insert_keys.reserve(c::BKEY_EXTENT_U64s_MAX as usize).is_ok()) {
                return Ok(());
            }
        }
    })();
    ret?;

    let more = op.wbio.bio.range().len() != 0;

    dst.bio(&mut op.wbio).raw_mut().bi_iter = saved_iter;

    if dst.is_op() && more {
        assert!(total_output == total_input, "a split of a bio we didn't bounce, its output not its input");
        dst = unsafe { ExtentBio::split(op, total_input >> 9) };
    }

    dst.bio(&mut op.wbio).raw_mut().bi_iter.bi_size = total_output;
    let reserved = reserved(&dst, &short);
    Ok(Extent { bio: dst, more, reserved })
}
