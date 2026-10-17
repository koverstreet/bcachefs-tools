//! Keys: their types, the forms they're held in, and narrowing between them.
//! types.rs is the types and traits - key types and families, the forms
//! they're held in, the macros generating them; bpos.rs, cmp.rs and text.rs
//! are positions, comparisons, and to and from text; here is the header, the
//! struct bkey every form has.
//!
//! **Value types and key types.** A key's type is its value's: c::Extent,
//! c::Dirent, the c::bch_<name> structs of BCH_BKEY_TYPES(). A key type
//! is a BkeyTag, the tag and minimum size, and a BkeyFamily, the set of just
//! it, whose Validated is the value as a key read in place
//! has it, as long as k.u64s says - and Unvalidated, the same from outside.
//! Three kinds of value, each declared at its definition:
//!  - plain (#[derive(SizedVal)]): every valid value is all of the struct -
//!    in place it's the struct, BkeyWhole, had by value; Unvalidated,
//!    UnvalidatedSized<the struct>;
//!  - grown (#[grown(.., bkey)]): an older value is shorter - in place it's
//!    the family's X<Validated>, the floor and the bytes there are; by value
//!    it's the struct, zero padded;
//!  - whole (#[whole(bkey)]): every valid value has the head, and maybe a tail -
//!    in place it's X<Validated>, as long as its u64s.
//! A key's state is its value's, and narrowing keeps it: an Unvalidated
//! key's arms are Unvalidated.
//! A key of any type's value is FamilyVal<AllTypes>, the family of every
//! type, the default V. BkeyVal is what a value type is - what V can be:
//! these, a family, a BkeyWhole's struct.
//!
//! **Key families.** BkeyFamily is a set of key types - what a key narrows
//! to: one type (every BkeyTag), or a family, the union of several, made
//! by bkey_family! from the BCH_BKEY_TYPES() rows a flag picks: Ptrs
//! (BKEY_TYPE_ptrs, data/ptrs.rs), Extents (BKEY_TYPE_extent,
//! data/extents.rs), Indirect (BKEY_TYPE_indirect, data/reflink.rs). A
//! family's value is read through typed(), the enum of its types - Typed's
//! arms but its own, PtrsTyped - and it can require more than the type: an
//! Extents key's size is nonzero. contains() decides.
//!
//! **Forms.** A key is held as a BkeySC<V> - borrowed, to read, Copy; a
//! BkeyS<V> - borrowed, to edit; a BkeyI<V> - header and value inline, by
//! reference, a DST as long as its u64s say; or a TransBkey<V> - in
//! transaction memory, to update (btree/iter.rs). Every form is a BkeyV -
//! its header, as_sc(), and arm() - and the editable ones BkeyVMut, as_s().
//! The same key in another form keeps V: From - BkeySC::from(&k),
//! BkeyS::from(&mut k) - or, generic over the form, as_sc(), as_s(). A key
//! converts to a family it's in by type, by From too, in its form or one it
//! converts to, in its state or a weaker one - never a stronger: one into()
//! does all three; untyped() erases keeping the form and state. as_ref() is
//! the BkeyI a validated key is: an unvalidated one has none.
//!
//! Conversions go one way: BkeyI (and TransBkey) -> BkeyS -> BkeySC. A
//! bkey_i's value is inline, after its header; a bkey_s's is split, a
//! pointer of its own, which may be anywhere - an unpacked key's value is
//! still in the btree node. So a BkeyS isn't a bkey_i: what needs one -
//! C's bch2_btree_insert() - takes a BkeyI.
//!
//! **Narrowing.** arm::<K>() narrows any form to K, keeping the form: None
//! if the key isn't one. It consumes the key, as a type change does; to
//! narrow a borrow, as_s().arm(). typed() on an untyped key - or a family's
//! - is the match on its type. Lookups narrow the same way:
//! peek_slot_typed::<K>(). arm() checks the tag, and a family's
//! constraints; a key's length is validate's, as in C.
//!
//! **Methods by value type.** A value type's trait says what its keys can
//! do - PtrsBkey, where its extent entries are - and a form trait gives it
//! to every form of key whose V has it: BkeyVPtrs (the entry walk),
//! BkeyVExtents (cut, on a key armed to Extents). bkey::prelude has the
//! form traits.
//!
//! **Copies.** MutBkey is what a mutable copy of a key read as V is: any
//! key's, as it is; a plain type's, itself; a grown type's, its whole struct,
//! grown (bkey_make_mut_noupdate()); a whole type's or a family's, itself,
//! as long as it is. bkey_reassemble() and bkey_reassemble_room() copy a
//! key keeping its V where the copy is still one (Mut = V). from_key() is
//! an owned copy of a plain or grown key, its u64s clamped to what it holds.

#![allow(non_camel_case_types)]

mod bpos;
mod cmp;
#[cfg(test)]
mod tests;
mod text;
mod types;

pub use self::bpos::*;
pub use self::cmp::*;
pub use self::text::*;
pub use self::types::*;

use crate::c;

pub const BKEY_U64S: usize = core::mem::size_of::<c::bkey>() / core::mem::size_of::<u64>();

pub type bkey_type = c::bch_bkey_type;

impl c::bkey {
    /// Of type deleted, at POS_MIN, with no value: as bkey_init() sets.
    pub fn new() -> Self {
        Self {
            u64s:        BKEY_U64S as u8,
            format_bits: c::bkey_format_bits::new().with_format(c::KEY_FORMAT_CURRENT as u8),
            ..Default::default()
        }
    }

    /// As its u64s say.
    pub fn val_bytes(&self) -> usize {
        (self.u64s as usize).saturating_sub(BKEY_U64S) * size_of::<u64>()
    }

    /// Whether it's @K's type: the tag. Not whether its value is long enough
    /// - that's validate's, as in C.
    pub fn is<K: BkeyTag + ?Sized>(&self) -> bool {
        self.type_ as u32 == K::TYPE.0
    }

    pub fn set_val_bytes(&mut self, bytes: usize) {
        let u64s = BKEY_U64S + bytes.div_ceil(size_of::<u64>());
        self.u64s = u8::try_from(u64s)
            .unwrap_or_else(|_| panic!("key of {u64s} u64s: a key is at most {} u64s", u8::MAX));
    }

    /// Make an extent @new_size long from where it starts: its end, its
    /// position, moves.
    pub fn resize(&mut self, new_size: u32) {
        self.p.offset = self.p.offset - self.size as u64 + new_size as u64;
        self.size = new_size;
    }

    pub fn set_range(&mut self, inode: u64, start: u64, end: u64, snapshot: u32) {
        self.p = spos(inode, end, snapshot);
        self.size = (end - start) as u32;
    }

    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.type_ as u32)
    }

    pub fn set_version_lo(&mut self, version: u64) {
        self.bversion.lo = version;
    }

    pub fn start_pos(&self) -> c::bpos {
        c::bpos { offset: self.start_offset(), ..self.p }
    }

    pub fn start_offset(&self) -> u64 {
        self.p.offset.wrapping_sub(self.size as u64)
    }

    pub fn is_deleted(&self) -> bool {
        self.key_type() == c::bch_bkey_type::KEY_TYPE_deleted
    }

    /// A tombstone in an extents btree.
    pub fn is_extent_whiteout(&self) -> bool {
        matches!(self.key_type(),
                 c::bch_bkey_type::KEY_TYPE_deleted |
                 c::bch_bkey_type::KEY_TYPE_whiteout |
                 c::bch_bkey_type::KEY_TYPE_extent_whiteout)
    }

    pub fn is_btree_ptr(&self) -> bool {
        matches!(self.key_type(),
                 c::bch_bkey_type::KEY_TYPE_btree_ptr | c::bch_bkey_type::KEY_TYPE_btree_ptr_v2)
    }
}
