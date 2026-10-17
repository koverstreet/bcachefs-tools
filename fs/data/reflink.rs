// SPDX-License-Identifier: GPL-2.0

//! Reflink: indirect extents - reflink_v, indirect_inline_data - in
//! BTREE_ID_reflink, refcounted, and the reflink_p keys in the extents btree
//! that point to them. data/reflink.c, as it's converted.
//!
//! An indirect extent is a key of the Indirect family, BKEY_TYPE_indirect: a
//! key narrows to one with arm::<Indirect>() - bkey_is_indirect(), and
//! reflink.c's bkey_extent_is_reflink_data() - and its refcount is read and
//! edited through it (BkeyVIndirect), in whichever form it's held.

use crate::btree::bkey::{bkey_family, BkeySC, BkeyV, BkeyVal, BkeyVMut};
use crate::c;
use crate::types::Le64;
use crate::util::vstructs::{cast_plain, cast_plain_mut, Validated};
use core::mem::offset_of;

/// A value with a refcount - of a BKEY_TYPE_indirect type, or Indirect,
/// either of them: where in it the refcount is.
pub trait IndirectBkey: BkeyVal {
    fn refcount_offset(k: BkeySC<'_, Self>) -> usize;
}

macro_rules! refcount_at {
    ($($ty:ident),*) => { $(
        impl IndirectBkey for c::$ty<Validated> {
            fn refcount_offset(_: BkeySC<'_, Self>) -> usize {
                offset_of!(c::$ty, repr.refcount)
            }
        }
    )* };
}
refcount_at!(ReflinkV, IndirectInlineData);

bkey_family!(Indirect);

macro_rules! indirect_bkey {
    ($(($name:ident, $nr:literal, $($rest:tt)*)),* $(,)?) => {
        impl IndirectBkey for Indirect {
            fn refcount_offset(k: BkeySC<'_, Indirect>) -> usize {
                match k.typed() {
                    $(IndirectTyped::$name(k) => IndirectBkey::refcount_offset(k),)*
                }
            }
        }
    };
}
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_indirect => indirect_bkey]);

/// The refcount of an indirect extent, in whichever form it's held: any BkeyV
/// whose value is an IndirectBkey - armed to Indirect, or to one of them.
pub trait BkeyVIndirect: BkeyV {
    /// bkey_refcount_c().
    fn refcount(&self) -> &Le64
    where Self::V: IndirectBkey
    {
        let k = self.as_sc();
        k.val_bytes().get(<Self::V as IndirectBkey>::refcount_offset(k)..)
            .and_then(cast_plain)
            .unwrap_or_else(|| panic!("{}: {} bytes, shorter than validate passes", k.k, k.val_bytes().len()))
    }

    /// bkey_refcount().
    fn refcount_mut(&mut self) -> &mut Le64
    where Self: BkeyVMut, Self::V: IndirectBkey
    {
        let k = *self.k();
        let at = <Self::V as IndirectBkey>::refcount_offset(self.as_sc());
        self.val_bytes_mut().get_mut(at..)
            .and_then(cast_plain_mut)
            .unwrap_or_else(|| panic!("{k}: {} bytes, shorter than validate passes", k.val_bytes()))
    }
}

impl<K: BkeyV> BkeyVIndirect for K {}

impl c::bkey {
    /// The type a direct extent of its type is, made indirect: deleted for a
    /// type that has none, as bkey_type_to_indirect() returns 0.
    pub fn indirect_type(&self) -> c::bch_bkey_type {
        use c::bch_bkey_type as t;

        match self.key_type() {
            t::KEY_TYPE_extent      => t::KEY_TYPE_reflink_v,
            t::KEY_TYPE_inline_data => t::KEY_TYPE_indirect_inline_data,
            _                       => t::KEY_TYPE_deleted,
        }
    }
}
