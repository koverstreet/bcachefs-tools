//! Extents: keys with a size, a range of their btree's offsets - what's
//! cut, merged and counted as file data. Their pointers, where they have
//! them, are data/ptrs.rs's.

use crate::btree::bkey::{bkey_family, BkeySC, BkeyV, BkeyVMut};
use crate::c;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::fs::Fs;

/*
 * BKEY_TYPE_extent flags the key types an extents btree may hold - checked
 * against BCH_BTREE_IDS(), which is what says: deleted, which every btree
 * may, and what each BTREE_IS_extents btree allows.
 */
macro_rules! extents_btrees_key_types {
    ($(($name:tt, $nr:literal, $($flag:ident)|+, $(BIT_ULL($t:ident))|+ $(, $($rest:tt)*)?)),* $(,)?) => {
        const EXTENTS_BTREES_KEY_TYPES: u64 =
            1 << c::bch_bkey_type::KEY_TYPE_deleted.0 $($(| 1 << c::bch_bkey_type::$t.0)+)*;
    };
}
c::BCH_BTREE_IDS!(cstruct_macros::xmacro_select [BTREE_IS_extents => extents_btrees_key_types]);

macro_rules! extent_types {
    ($(($name:ident, $nr:literal, $($rest:tt)*)),* $(,)?) => {
        const EXTENT_KEY_TYPES: u64 = 0 $(| 1 << $nr)*;
    };
}
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_extent => extent_types]);

bkey_family!(Extents);

const _: () = assert!(EXTENT_KEY_TYPES == EXTENTS_BTREES_KEY_TYPES,
                      "BKEY_TYPE_extent isn't on exactly the key types BCH_BTREE_IDS()' extents btrees hold");

impl c::bkey {
    /// Whether it's counted in its inode's i_sectors.
    pub fn extent_is_allocation(&self) -> bool {
        use c::bch_bkey_type as t;

        matches!(self.key_type(),
                 t::KEY_TYPE_extent | t::KEY_TYPE_reservation | t::KEY_TYPE_reflink_p |
                 t::KEY_TYPE_reflink_v | t::KEY_TYPE_inline_data |
                 t::KEY_TYPE_indirect_inline_data | t::KEY_TYPE_error)
    }

    /// Whether its value is pointers to its data, and not to another key's.
    pub fn extent_is_direct_data(&self) -> bool {
        use c::bch_bkey_type as t;

        matches!(self.key_type(),
                 t::KEY_TYPE_btree_ptr | t::KEY_TYPE_btree_ptr_v2 | t::KEY_TYPE_extent |
                 t::KEY_TYPE_reflink_v)
    }
}

impl BkeySC<'_> {
    /// Whether it has space allocated and not yet written: an unwritten
    /// pointer.
    pub fn extent_is_unwritten(self, fs: &Fs) -> bool {
        self.arm::<Ptrs>().is_some_and(|k| k.ptrs(fs).any(|p| p.unwritten() != 0))
    }

    /// Whether it reserves space rather than holding data - a reservation, or
    /// an extent with unwritten pointers.
    pub fn extent_is_reservation(self, fs: &Fs) -> bool {
        self.k.key_type() == c::bch_bkey_type::KEY_TYPE_reservation || self.extent_is_unwritten(fs)
    }

}

/*
 * A cut that leaves nothing - which makes the key a size 0 deletion - gives
 * back None, not an empty extent: what's left isn't to be inserted, and the
 * caller branches on it. One that leaves anything leaves its type, so it's
 * still an extent.
 */

/// What an extent, armed to Extents, does in whichever form it's held.
pub trait BkeyVExtents: BkeyV {
    /// Cut to start at @where_: bch2_cut_front_s().
    fn cut_front(mut self, fs: &Fs, where_: c::bpos) -> Option<Self>
    where Self: BkeyVMut<V = Extents>
    {
        unsafe { c::bch2_cut_front_s(fs.as_ptr(), where_, self.as_s().as_raw()) };
        (self.k().size != 0).then_some(self)
    }

    /// Cut to end at @where_: bch2_cut_back_s().
    fn cut_back(mut self, where_: c::bpos) -> Option<Self>
    where Self: BkeyVMut<V = Extents>
    {
        unsafe { c::bch2_cut_back_s(where_, self.as_s().as_raw()) };
        (self.k().size != 0).then_some(self)
    }
}

impl<K: BkeyV> BkeyVExtents for K {}
