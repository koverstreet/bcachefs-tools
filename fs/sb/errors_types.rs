// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::DArray;
use cstruct_macros::{CStruct, bitfield, c_typedef, c_xmacro, TypeInfo};
use nestify::nest;

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
    pub struct bch_sb_error_entry_cpu {
        #[c_bitfield]
        #>[bitfield(u64)]
        pub bits: pub struct bch_sb_error_entry_cpu_bits {
            id: u16,
            nr: u48,
        },
        pub first_error_time: u64,
        pub last_error_time: u64,
    }
}

c_typedef! {
    #[c("DARRAY(struct bch_sb_error_entry_cpu) bch_sb_errors_cpu")]
    pub type bch_sb_errors_cpu = DArray<c::bch_sb_error_entry_cpu>;
}

/*
 * Block statuses we count separately: what the device said is the first
 * thing worth knowing from a filesystem that's logging IO errors - media
 * errors mean the drive is going, timeouts mean it's hanging, transport
 * errors point at the fabric, and INVAL means the request itself was
 * rejected: not an error the device hit, an error in what we handed it.
 *
 * Curated by hand rather than generated from BLK_ERRS(): sb error ids are
 * permanent on-disk numbers, so we don't spend them on statuses that can't
 * reach a bcachefs completion - DM_REQUEUE is consumed inside dm, and the
 * zone codes want zoned device support we don't have. Anything left out
 * counts as blk_sts_unknown; to promote one, add it here and add a
 * numbered entry in errors_format.rs - the build breaks if you forget.
 */
c_xmacro! {
    BCH_BLK_STS_SB_ERRS(x) {
        (NOTSUPP, notsupp),
        (TIMEOUT, timeout),
        (NOSPC, nospc),
        (TRANSPORT, transport),
        (TARGET, target),
        (RESV_CONFLICT, resv_conflict),
        (MEDIUM, medium),
        (PROTECTION, protection),
        (RESOURCE, resource),
        (IOERR, ioerr),
        (AGAIN, again),
        (DEV_RESOURCE, dev_resource),
        (OFFLINE, offline),
        (DURATION_LIMIT, duration_limit),
        (INVAL, inval),
        (REMOVED, removed),
    }
}

/*
 * Decompression failures we count separately: an extent that won't
 * decompress is a different animal from one that won't read or won't
 * checksum - as far as the checksum can tell the data came off the device
 * intact, and then didn't decode. Counting per compression type answers
 * the first question such a report raises, which is whether one algorithm
 * is at fault or all of them are.
 *
 * The zstd codes separate that further: corruption_detected and
 * checksum_wrong say the compressed data is bad, dst_size_too_small and
 * memory_allocation say we drove zstd wrong or couldn't allocate.
 *
 * Curated by hand for the same reason as the block statuses above - sb
 * error ids are permanent on-disk numbers. zstd defines 27 error codes,
 * most of which zstd_decompress_dctx() can't return for the single raw
 * frame we hand it; those count as data_decompress_err_zstd_unknown, and
 * anything not a decompress errcode at all as data_decompress_err_unknown.
 * To promote one, add it here and add a numbered entry in
 * errors_format.rs, and a KEY_TYPE_ERRORS() reason - the build breaks if
 * you forget.
 */
c_xmacro! {
    BCH_DECOMPRESS_SB_ERRS(x) {
        (decompress_exceeded_max_encoded_extent, exceeded_max_encoded_extent),
        (decompress_lz4_old, lz4_old),
        (decompress_lz4, lz4),
        (decompress_gzip, gzip),
        (decompress_gzip_size_mismatch, gzip_size_mismatch),
        (decompress_zstd_src_len_bad, zstd_src_len_bad),
        (decompress_zstd_size_mismatch, zstd_size_mismatch),
        (ZSTD_error_corruption_detected, zstd_corruption_detected),
        (ZSTD_error_checksum_wrong, zstd_checksum_wrong),
        (ZSTD_error_prefix_unknown, zstd_prefix_unknown),
        (ZSTD_error_srcSize_wrong, zstd_src_size_wrong),
        (ZSTD_error_dstSize_tooSmall, zstd_dst_size_too_small),
        (ZSTD_error_memory_allocation, zstd_memory_allocation),
    }
}
