// SPDX-License-Identifier: GPL-2.0

//! The data types of data/compress_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

/*
 * Compressing and decompressing want wildly different amounts of scratch
 * space, so they get separate pools: a zstd compression context carries the
 * match finder's hash and chain tables, sized by the compression level, while
 * decompression carries only the huffman and FSE decode tables - it doesn't
 * search, it follows offsets. One pool sized to the larger would make every
 * read pay the write side's footprint.
 *
 * Both are indexed by compression type rather than shared, because a type can
 * be enabled after mount (bch2_check_set_has_compressed_data()) and a mempool
 * cannot grow.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_compress {
    pub bounce: [c::mempool_t; 2],
    pub workspace: [c::mempool_t; c::BCH_COMPRESSION_OPT_NR as usize],
    pub decompress_workspace: [c::mempool_t; c::BCH_COMPRESSION_OPT_NR as usize],
    pub zstd_workspace_size: usize,
}
c_default!(bch_fs_compress);
