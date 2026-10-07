// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/bkey_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{c_enum, c_typedef, c_verbatim, c_xmacro, CStruct};

c_enum! {
    #[closed]
    pub enum bkey_lr_packed: u32 {
        BKEY_PACKED_BOTH,
        BKEY_PACKED_RIGHT,
        BKEY_PACKED_LEFT,
        BKEY_PACKED_NONE,
    }
}

c_verbatim!(r#"
#define bkey_lr_packed(_l, _r)						\
	((_l)->format + ((_r)->format << 1))
"#);

/*
 * Wrapper for stack-allocated struct bkey_packed.
 *
 * The byte-aligned fast-path unpackers (__bch2_bkey_unpack_key_b,
 * __bkey_unpack_pos_b) issue an 8-byte unaligned load at @bytes +
 * uf->byte_offset, where byte_offset is signed and can be as low as -7
 * (and __bch2_bkey_unpack_key_b's header trick reads @bytes - 1). This
 * is safe for bkeys inside a bset (preceded by the bset header or by
 * another bkey), but reads into the stack redzone for a bare stack-local
 * struct bkey_packed. Wrap stack copies in this to provide the leading
 * padding.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bkey_packed_padded {
    pub _pad: [u8; 8],
    pub k: c::bkey_packed,
}

/* Same trick for stack-local struct bkey_i (when used as pack target). */
#[repr(C)]
#[derive(Default, CStruct)]
pub struct bkey_i_padded {
    pub _pad: [u8; 8],
    pub k: c::bkey_i,
}

c_verbatim!(r#"
struct btree;
"#);

#[cfg(not(CONFIG_BCACHEFS_DEBUG))]
c_verbatim!(r#"
#define bkey_packed(_k)		((_k)->format != KEY_FORMAT_CURRENT)
"#);

c_enum! {
    #[closed]
    pub enum bkey_pack_pos_ret: u32 {
        BKEY_PACK_POS_EXACT,
        BKEY_PACK_POS_SMALLER,
        BKEY_PACK_POS_FAIL,
    }
}

c_typedef! {
    pub type compiled_unpack_fn = Option<unsafe extern "C" fn(*mut c::bkey, *const c::bkey_packed)>;
}

c_xmacro! {
    bkey_fields(x) {
        (BKEY_FIELD_INODE, p.inode),
        (BKEY_FIELD_OFFSET, p.offset),
        (BKEY_FIELD_SNAPSHOT, p.snapshot),
        (BKEY_FIELD_SIZE, size),
        (BKEY_FIELD_VERSION_HI, bversion.hi),
        (BKEY_FIELD_VERSION_LO, bversion.lo),
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bkey_format_state {
    pub field_min: [u64; c::BKEY_NR_FIELDS as usize],
    pub field_max: [u64; c::BKEY_NR_FIELDS as usize],
}
