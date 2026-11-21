// SPDX-License-Identifier: GPL-2.0

//! A write's data made into extents at its write point: bch2_write_extent(),
//! with the bios and encoding that takes.

use crate::alloc::sector_alloc::LockedWritePoint;
use crate::util::alloc::flags;
use crate::c;
use crate::data::compress::{bio_bounce, bio_compress, bounce_alloc, buf_uncompress, decompress_err, map_or_bounce, Src};
use crate::data::ptrs::{crc_is_compressed, extent_crc_append, extent_nonce};
use crate::data::write::{write_op_error, ExtentBio, ExtentData};
use crate::data::write_replicas::wbio_in_place;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::locking::MemallocFlags;
use crate::util::bio::{BioBuffersOwned, BioRange};
use crate::util::volatile::Out;
use crate::os::kernel::{unlikely, PAGE_SIZE, WRITE};
use c::WriteFlags as w;
use core::ffi::c_void;
use core::ptr;
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

/// A bio of the extent's own for @len bytes of the op's data, written with
/// @opf: bounced into pages of its own, or the write point's stripe buffer.
fn write_bio_alloc(fs: &'static Fs, wp: &LockedWritePoint<'_>, len: u32, opf: u32, short: &mut AllocShort,
                   buf: *mut c_void) -> ExtentBio {
    let mut output_available = (wp.sectors_free() << 9).min(len);
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

    if !buf.is_null() {
        let buf = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), output_available as usize) };
        return ExtentBio::Stripe(fs.bio_write.alloc_with(ptr::null_mut(), opf, buf, wbio_in_place)
            .unwrap_or_else(|e: BchError| panic!("a write's bio for its stripe: its bioset waits, but {e}")));
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
    let bounce = {
        let _noio = MemallocFlags::noio();
        fs.bio_write.alloc_with::<BioBuffersOwned<'static>, BchError, _>(
            ptr::null_mut(), opf,
            (&fs.bio_bounce, block_size, output_available as usize, required as usize, flags::NOIO | flags::SKIP_ZERO),
            wbio_in_place)
    }.unwrap_or_else(|e: BchError| panic!("a write's bounce pages, outside NORECLAIM: {e}"));
    short.pool  = bounce.buffers().used_pool();
    short.pages = bounce.bio.range().len() < output_available;
    ExtentBio::Bounce(bounce)
}

fn write_rechecksum(fs: &Fs, op: &mut Op, data: &mut BioRange<'_>, mut new_csum_type: c::CsumType)
    -> Result<(), BchError>
{
    let mut new_crc = c::bch_extent_crc_unpacked::default();

    /* bch2_rechecksum_bio() can't encrypt or decrypt data: */

    if op.crc.csum_type.get().is_encryption() != new_csum_type.is_encryption() {
        new_csum_type = op.crc.csum_type.get();
    }

    fs.rechecksum_bio(*data, op.version, op.crc, None, Some(&mut new_crc),
                      op.crc.offset.into(), op.crc.live_size, new_csum_type)?;

    data.advance(u32::from(op.crc.offset) << 9);
    *data = data.take(op.crc.live_size << 9);
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
fn write_op_decode(op: &mut Op, data: &mut BioRange<'_>, update: Option<&c::data_update_index>)
    -> Result<(), BchError>
{
    let fs = op.fs();
    let crc = op.crc;
    let dst_len = (crc.uncompressed_size as usize) << 9;

    assert!(data.len() == crc.compressed_size << 9, "decoding a bio that isn't its extent");

    if crc.uncompressed_size << 9 > fs.opts().encoded_extent_max.get() {
        write_op_error(op, update, op.pos.offset, format_args!("extent too big to decompress ({} > {})",
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
    let range = *data;
    let mut src_buf = if crc.csum_type.get().is_encryption() {
        let mut b = bio_bounce(fs, range);
        fs.encrypt(crc.csum_type.get(), extent_nonce(op.version, &crc), &mut b[..(crc.compressed_size as usize) << 9])?;
        Src::Bounced(b)
    } else {
        map_or_bounce(fs, range)
    };

    let ret = buf_uncompress(fs, Out::from(&mut *dst_buf), src_buf.data(), &crc);
    drop(src_buf);
    let decoded = match ret {
        Ok(data) => Some(data),
        // nothing was read: there's nothing to copy
        Err(_) if fs.opts().no_data_io.get() != 0 => None,
        Err(e) => {
            write_op_error(op, update, op.pos.offset, format_args!("{e}"));
            return decompress_err(fs, Err(e));
        }
    };

    /* in the bio's own pages, which have room for all of it */
    *data = data.resize(crc.live_size << 9);

    if let Some(decoded) = decoded {
        let offset = usize::from(crc.offset) << 9;
        data.copy_from_volatile(decoded.sub(offset..offset + ((crc.live_size as usize) << 9)));
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

fn csum_err(op: &Op, update: Option<&c::data_update_index>, csum: c::bch_csum) -> BchError {
    let fs = op.fs();
    let ty = op.crc.csum_type.get();
    let ty_name: &dyn core::fmt::Display = if ty.0 < c::CsumType::NR { &ty } else { &"(unknown)" };
    write_op_error(op, update, op.pos.offset, format_args!(
        "error verifying existing checksum while moving existing data (memory corruption?)\n  \
         expected {:0x}:{:0x} got {:0x}:{:0x} type {}",
        op.crc.csum.hi.get(), op.crc.csum.lo.get(), csum.hi.get(), csum.lo.get(), ty_name));
    fs.err(Errcode::data_write_csum)
}

#[inline(never)]
fn write_prep_encoded_data(op: &mut Op, data: &mut BioRange<'_>, update: Option<&c::data_update_index>,
                           wp: &LockedWritePoint<'_>)
    -> Result<(), BchError>
{
    let fs = op.fs();

    assert!(data.len() >> 9 == op.crc.compressed_size, "an encoded write's bio isn't its extent");

    /* Can we just write the entire extent as is? */
    if op.crc.uncompressed_size == op.crc.live_size &&
       op.crc.uncompressed_size <= fs.opts().encoded_extent_max.get() >> 9 &&
       op.crc.compressed_size <= wp.sectors_free() &&
       op.crc.csum_type.get().is_encryption() == op.bits.csum_type().is_encryption() &&
       (op.crc.compression_type == c::bch_compression_opt::to_type(op.bits.compression_opt(), fs) ||
        op.bits.incompressible() != 0) {
        if op.crc.csum_type != op.bits.csum_type() {
            if !crc_is_compressed(&op.crc) {
                write_rechecksum(fs, op, data, op.bits.csum_type())?;
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
                let new_csum = fs.checksum_range(op.bits.csum_type(), nonce, *data);

                let csum = fs.checksum_range(op.crc.csum_type.get(), nonce, *data);
                if csum_differs(op.crc.csum, csum) && fs.opts().no_data_io.get() == 0 {
                    return Err(csum_err(op, update, csum));
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
        let csum = fs.checksum_range(op.crc.csum_type.get(), nonce, *data);
        if csum_differs(op.crc.csum, csum) && fs.opts().no_data_io.get() == 0 {
            return Err(csum_err(op, update, csum));
        }

        /*
         * Decryption happens inside bch2_write_op_decode(), on
         * its own copy - @bio and @op->crc are left exactly as they are
         * until the decompressed data is ready to go back. So if the
         * data won't decompress there is nothing to undo, and writing
         * the extent as we found it is still on the table:
         */
        if let Err(e) = write_op_decode(op, data, update) {
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
        write_rechecksum(fs, op, data, op.bits.csum_type())?;
    }

    /*
     * If we want to compress the data, it has to be decrypted:
     */
    if op.crc.csum_type.get().is_encryption() &&
       (op.bits.compression_opt() != 0 || op.crc.csum_type != op.bits.csum_type()) {
        let nonce = extent_nonce(op.version, &op.crc);
        let csum = fs.checksum_range(op.crc.csum_type.get(), nonce, *data);
        if csum_differs(op.crc.csum, csum) && fs.opts().no_data_io.get() == 0 {
            return Err(csum_err(op, update, csum));
        }

        fs.encrypt_range(nonce, *data)?;

        op.crc.csum_type.set(c::CsumType::none);
        op.crc.csum = c::bch_csum::default();
    }

    Ok(())
}

pub(crate) struct Extent<'a> {
    pub data:     ExtentData<'a>,
    /// Of the op's data, after it
    pub more:     bool,
    /// Its memory is - or may be - a mempool's reserve: the write mustn't
    /// ask for more till it's freed
    pub reserved: bool,
}

/// The next of @op's data - @data, a position in its bio, advanced past it -
/// written out at @wp: its extents' keys appended, and what to submit - a bio
/// of its own made with @opf - or an error, and no keys appended.
pub(crate) fn write_extent<'a>(op: &mut Op, data: &mut BioRange<'a>, opf: u32,
                               update: Option<&c::data_update_index>, wp: &mut LockedWritePoint<'_>)
    -> Result<Extent<'a>, BchError>
{
    let fs = op.fs();
    /* a bio of its own - bounced, or the stripe's - or none: written in place */
    let mut dst: Option<ExtentBio> = None;
    let (mut total_output, mut total_input) = (0u32, 0u32);
    let mut short = AllocShort::default();
    /* the bounce pool's, or a bio of bio_write's while its one reserve bio is out */
    let reserved = |e: &ExtentData<'_>, short: &AllocShort|
        short.pool || (matches!(e, ExtentData::Own(_)) && fs.bio_write.reserve_drawn());
    /* what it has room for still: its own bio's, or, in place, the data's */
    let room = |dst: &Option<ExtentBio>, data: &BioRange<'_>| dst.as_ref().map_or(data.len(), |d| d.range().len());

    if op.bits.incompressible() != 0 {
        op.bits.set_compression_opt(0);
    }

    assert!(data.len() >> 9 != 0, "writing an empty bio");

    let ec_buf = wp.ec_buf();

    if unlikely(op.has(w::data_encoded)) {
        match write_prep_encoded_data(op, data, update, wp) {
            Err(e) if e.matches(Errcode::write_encoded_as_is) => {
                let all = *data;
                data.advance(all.len());
                let extent = if !ec_buf.is_null() {
                    let stripe = write_bio_alloc(fs, wp, all.len(), opf, &mut short, ec_buf);
                    stripe.range().copy_from_range(all);
                    ExtentData::Own(stripe)
                } else {
                    ExtentData::InPlace(all)
                };
                init_append_extent(op, wp, op.version, op.crc);
                let reserved = reserved(&extent, &short);
                return Ok(Extent { data: extent, more: false, reserved });
            }
            Err(e) => return Err(e),
            Ok(()) => {}
        }
    }

    if !ec_buf.is_null() ||
       op.bits.compression_opt() != 0 ||
       (op.bits.csum_type() != c::CsumType::none && !op.has(w::pages_stable)) ||
       (op.bits.csum_type().is_encryption() && !op.has(w::pages_owned)) {
        dst = Some(write_bio_alloc(fs, wp, data.len(), opf, &mut short, ec_buf));
    }

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    let write_corrupt_ratio = crate::data::write::write_corrupt_ratio.get();
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    if dst.is_none() && write_corrupt_ratio != 0 {
        dst = Some(write_bio_alloc(fs, wp, data.len(), opf, &mut short, ec_buf));
    }
    let bounce = dst.is_some();
    let start = *data;
    let saved_iter = dst.as_ref().map(|d| d.range().pos());

    let ret = (|| -> Result<(), BchError> {
        loop {
            let mut crc = c::bch_extent_crc_unpacked::default();
            let mut version = op.version;
            let (mut dst_len, mut src_len) = (0u32, 0u32);
            let src_size = data.len();

            assert!(u64::from(src_size) & (fs.block_bytes() - 1) == 0,
                    "a write of {src_size} bytes, not whole blocks");

            if short.pages &&
               room(&dst, data) < wp.sectors_free() << 9 &&
               room(&dst, data) < fs.opts().encoded_extent_max.get() {
                return Ok(());
            }

            assert!(!(op.bits.compression_opt() != 0 && op.has(w::data_encoded) &&
                      op.crc.csum_type.get().is_encryption()), "compressing data still encrypted");
            assert!(!(op.bits.compression_opt() != 0 && !bounce), "compressing without bouncing");

            crc.compression_type.set(if op.bits.incompressible() != 0 {
                c::CompressionType::incompressible
            } else if op.bits.compression_opt() != 0 {
                let (mut d, mut s) = (0usize, 0usize);
                let dst_r = dst.as_ref().expect("compressing into a bio of its own").range();
                let t = bio_compress(fs, dst_r, &mut d, *data, &mut s, op.bits.compression_opt().into(),
                                     op.pos, !op.has(w::pages_stable));
                (dst_len, src_len) = (d as u32, s as u32);
                t
            } else {
                c::CompressionType::none
            });

            if !crc_is_compressed(&crc) {
                dst_len = room(&dst, data).min(src_size);
                dst_len = dst_len.min(wp.sectors_free() << 9);

                if op.bits.csum_type() != c::CsumType::none {
                    dst_len = dst_len.min(fs.opts().encoded_extent_max.get());
                }

                if let Some(d) = &dst {
                    d.range().take(dst_len).copy_from_range(*data);
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
                fs.rechecksum_bio(*data, version, old,
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
                    fs.rechecksum_bio(*data, version, old,
                                      None, Some(&mut op.crc),
                                      src_len >> 9,
                                      (src_size >> 9) - (src_len >> 9),
                                      old.csum_type.get())?;
                }

                crc.compressed_size   = dst_len >> 9;
                crc.uncompressed_size = src_len >> 9;
                crc.live_size         = src_len >> 9;

                let extent = dst.as_ref().map_or(*data, |d| d.range()).take(dst_len);
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
                let mut b = dst.as_mut().expect("corrupting a bio of its own").bio_mut();
                let s = b.range().len();
                b.as_mut().raw_mut().bi_iter.bi_size = dst_len;
                b.as_mut().corrupt();
                b.as_mut().raw_mut().bi_iter.bi_size = s;
            }

            if let Some(d) = &mut dst {
                d.bio_mut().advance(dst_len);
            }
            data.advance(src_len);
            total_output += dst_len;
            total_input  += src_len;

            if !(room(&dst, data) != 0 &&
                 data.len() != 0 &&
                 wp.sectors_free() != 0 &&
                 op.insert_keys.reserve(c::BKEY_EXTENT_U64s_MAX as usize).is_ok()) {
                return Ok(());
            }
        }
    })();
    let more = match ret {
        Ok(()) => data.len() != 0,
        Err(e) if total_input == 0 => return Err(e),
        // keys appended for the pieces before it: those go out, and the write
        // ends there, failed - the rest not written, its extent left as it was
        Err(e) => {
            op.set_error(e);
            false
        }
    };

    let extent = match dst.zip(saved_iter) {
        Some((mut d, iter)) => {
            let mut b = d.bio_mut();
            b.as_mut().raw_mut().bi_iter = iter;
            b.as_mut().raw_mut().bi_iter.bi_size = total_output;
            ExtentData::Own(d)
        }
        None => {
            assert!(total_output == total_input, "an extent written in place, its output not its input");
            ExtentData::InPlace(start.take(total_input))
        }
    };
    let reserved = reserved(&extent, &short);
    Ok(Extent { data: extent, more, reserved })
}
