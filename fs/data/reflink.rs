// SPDX-License-Identifier: GPL-2.0

//! Reflink: indirect extents - reflink_v, indirect_inline_data - in
//! BTREE_ID_reflink, refcounted, and the reflink_p keys in the extents btree
//! that point to them. data/reflink.c, as it's converted.

use crate::btree::bkey::{BkeyI, BkeyITypedMut, BkeySC, BkeySCTyped};
use crate::c;
use crate::types::Le64;

impl c::bkey {
    /// Whether it's an indirect extent - what a reflink_p points to:
    /// bkey_is_indirect(), and reflink.c's bkey_extent_is_reflink_data().
    pub fn is_indirect(&self) -> bool {
        matches!(self.key_type(),
                 c::bch_bkey_type::KEY_TYPE_reflink_v | c::bch_bkey_type::KEY_TYPE_indirect_inline_data)
    }

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

impl<'a> BkeySC<'a> {
    /// An indirect extent's refcount: bkey_refcount_c(). None for any other
    /// key.
    pub fn refcount(self) -> Option<&'a Le64> {
        match self.typed() {
            BkeySCTyped::reflink_v(r)            => Some(&r.v().refcount),
            BkeySCTyped::indirect_inline_data(d) => Some(&d.v().refcount),
            _                                    => None,
        }
    }
}

impl BkeyI {
    /// refcount(), to edit: bkey_refcount().
    pub fn refcount_mut(&mut self) -> Option<&mut Le64> {
        match self.typed_mut() {
            BkeyITypedMut::reflink_v(r)            => Some(&mut r.v_mut().refcount),
            BkeyITypedMut::indirect_inline_data(d) => Some(&mut d.v_mut().refcount),
            _                                      => None,
        }
    }
}
