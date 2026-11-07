//! What C calls of btree/bkey: key formats, and packing keys to and from one.
//! A key C passes in is whole, as long as its u64s say; one packed for C is as
//! many words as its format says. A node C passes is locked, so its contents
//! - its format - can be read.

use core::ffi::c_int;
use core::mem::offset_of;
use core::slice;

use super::{transform, BkeyP, BKEY_FORMAT_CURRENT, BKEY_U64S};
use crate::btree::node::BtreeNode;
use crate::c;
use crate::format::Bpos;
use crate::util::vstructs::plain_bytes_mut;
use crate::util::{Printbuf, ToText};
use cstruct_macros::c_export;

unsafe fn words_mut<'a>(k: *mut c::bkey_packed, u64s: usize) -> &'a mut [u64] {
    unsafe { slice::from_raw_parts_mut(k.cast(), u64s) }
}

/*
 * For the fast paths: a key C has is in a node's buffer or a
 * bkey_packed_padded, so the word before it is readable too.
 */
unsafe fn padded<'a>(k: *const c::bkey_packed) -> &'a [u64] {
    unsafe { slice::from_raw_parts(k.cast::<u64>().sub(1), 1 + (*k).u64s as usize) }
}

unsafe fn padded_mut<'a>(k: *mut c::bkey_packed, u64s: usize) -> &'a mut [u64] {
    unsafe { slice::from_raw_parts_mut(k.cast::<u64>().sub(1), 1 + u64s) }
}

#[c_export]
pub static bch2_bkey_format_current: c::bkey_format = BKEY_FORMAT_CURRENT;

#[c_export]
pub fn bch2_bkey_format_init(s: &mut c::bkey_format_state) {
    *s = c::bkey_format_state::new();
}

#[c_export]
pub fn bch2_bkey_format_add_pos(s: &mut c::bkey_format_state, p: Bpos) {
    s.add_pos(p);
}

#[c_export]
pub fn bch2_bkey_format_add_key(s: &mut c::bkey_format_state, k: &c::bkey) {
    s.add_key(k);
}

#[c_export]
pub fn bch2_bkey_format_done(s: &mut c::bkey_format_state) -> c::bkey_format {
    s.done()
}

#[cold]
#[c_export]
pub fn bch2_bkey_format_to_text(out: &mut Printbuf, f: &c::bkey_format) {
    f.to_text(out);
}

#[c_export]
pub unsafe fn __bch2_bkey_unpack_key(format: &c::bkey_format, out: *mut c::bkey, in_: *const c::bkey_packed) {
    unsafe { out.write(format.unpack_key(&*BkeyP::from_raw(in_))) }
}

#[c_export]
pub unsafe fn __bch2_bkey_unpack_key_b(b: &BtreeNode, out: *mut c::bkey, in_: *const c::bkey_packed) {
    let b = unsafe { b.contents() };
    unsafe { out.write(b.unpack_key_at(padded(in_), 1)) }
}

#[c_export]
pub unsafe fn __bkey_unpack_pos_b(b: &BtreeNode, in_: *const c::bkey_packed) -> Bpos {
    let b = unsafe { b.contents() };
    b.unpack_pos_at(unsafe { padded(in_) }, 1)
}

#[c_export]
pub unsafe fn bch2_bkey_pack_pos(out: *mut c::bkey_packed, in_: Bpos, b: &BtreeNode) -> bool {
    let b = unsafe { b.contents() };
    b.pack_pos(unsafe { padded_mut(out, b.format.key_u64s as usize) }, 1, in_)
}

#[c_export]
pub unsafe fn bch2_bkey_transform(out_f: &c::bkey_format, out: *mut c::bkey_packed,
                                  in_f: &c::bkey_format, in_: *const c::bkey_packed) -> bool {
    let in_ = unsafe { &*BkeyP::from_raw(in_) };
    let val_u64s = in_.u64s() - in_f.key_u64s as usize;

    transform(out_f, unsafe { words_mut(out, out_f.key_u64s as usize + val_u64s) }, in_f, in_)
}

#[c_export]
pub unsafe fn bch2_bkey_unpack(b: &BtreeNode, dst: *mut c::bkey_i, src: *const c::bkey_packed) {
    let b = unsafe { b.contents() };
    let k = unsafe { &*BkeyP::from_raw(src) };
    let u = b.unpack_key_at(unsafe { padded(src) }, 1);
    let u64s = BKEY_U64S + k.val_u64s(&b.format);

    k.unpacked_into(&b.format, u, unsafe { slice::from_raw_parts_mut(dst.cast(), u64s) });
}

#[c_export]
pub unsafe fn bch2_bkey_cmp_packed(b: &BtreeNode, l: *const c::bkey_packed, r: *const c::bkey_packed) -> c_int {
    let b = unsafe { b.contents() };
    unsafe { b.cmp_packed(&*BkeyP::from_raw(l), &*BkeyP::from_raw(r)) as c_int }
}

#[c_export]
pub unsafe fn __bch2_bkey_cmp_left_packed(b: &BtreeNode, l: *const c::bkey_packed, r: &Bpos) -> c_int {
    let b = unsafe { b.contents() };
    b.unpack_pos_at(unsafe { padded(l) }, 1).cmp(r) as c_int
}

#[c_export]
pub fn bch2_bpos_swab(p: &mut Bpos) {
    p.swab();
}

pub(crate) unsafe fn bkey_swab_key(f: Option<&c::bkey_format>, k: *mut c::bkey_packed) {
    let f = if unsafe { (*BkeyP::from_raw(k)).is_packed() } {
        f.expect("a packed key to swab, with no format")
    } else {
        &BKEY_FORMAT_CURRENT
    };

    plain_bytes_mut(unsafe { words_mut(k, f.key_u64s as usize) })[offset_of!(c::bkey_packed, key_start)..].reverse();
}
