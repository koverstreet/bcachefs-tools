// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{bch_enum, xmacro, MaybeZeroable, Plain};

/*
 * Wrapper for stack-allocated struct bkey_packed.
 *
 * The byte-aligned fast paths (BtreeNode::unpack_key_at(), unpack_pos_at(),
 * pack_pos()) issue an 8-byte unaligned load at @bytes +
 * uf->byte_offset, where byte_offset is signed and can be as low as -7
 * (and unpack_key_at()'s header trick reads @bytes - 1). This
 * is safe for bkeys inside a bset (preceded by the bset header or by
 * another bkey), but reads into the stack redzone for a bare stack-local
 * struct bkey_packed. Wrap stack copies in this to provide the leading
 * padding.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain)]
pub struct bkey_packed_padded {
    pub _pad: [u8; 8],
    pub k: c::bkey_packed,
}

#[bch_enum(closed)]
#[repr(u32)]
pub enum BkeyPackPosRet {
    BKEY_PACK_POS_EXACT,
    BKEY_PACK_POS_SMALLER,
    BKEY_PACK_POS_FAIL,
}

#[allow(non_camel_case_types)]
pub type compiled_unpack_fn = Option<unsafe extern "C" fn(*mut c::bkey, *const c::bkey_packed)>;

xmacro! {
    bkey_fields {
        (BKEY_FIELD_INODE, p.inode),
        (BKEY_FIELD_OFFSET, p.offset),
        (BKEY_FIELD_SNAPSHOT, p.snapshot),
        (BKEY_FIELD_SIZE, size),
        (BKEY_FIELD_VERSION_HI, bversion.hi),
        (BKEY_FIELD_VERSION_LO, bversion.lo),
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct bkey_format_state {
    pub field_min: [u64; c::BkeyField::BKEY_NR_FIELDS as usize],
    pub field_max: [u64; c::BkeyField::BKEY_NR_FIELDS as usize],
}
