//! Packing keys into a format and back: a packed key's fields are a bit
//! stream from its high word down, each its bits above the format's offset.
//! A key is taken here as its words, from its header on.

use core::cmp::Ordering;
use core::mem::offset_of;

use super::format::{fls64, u64_bitmask, BKEY_FORMAT_CURRENT, FIELDS, NR_FIELDS};
use super::packed::Header;
use super::{spos, BkeyP, BKEY_U64S};
use crate::btree::node::{BkeyUnpackField, NodeContents};
use crate::c;
use crate::cstructs::c::{bkey_fields, bkey_pack_pos_ret};
use crate::debug::params::debug_check_bkey_unpack;
use crate::format::bch_bkey_fields::{self, *};
use crate::format::{bkey_format, Bpos};
use crate::util::kernel::unlikely;
use crate::util::vstructs::{cast_plain_slice_mut, plain_bytes, plain_bytes_mut};
use crate::util::{Printbuf, ToText};

const POS_FIELDS: [bch_bkey_fields; 3] = [BKEY_FIELD_INODE, BKEY_FIELD_OFFSET, BKEY_FIELD_SNAPSHOT];

/// Where a packed key's position bits start in its high word: past the header,
/// on big endian.
pub const HIGH_BIT_OFFSET: u32 = if cfg!(target_endian = "little") { 0 } else { c::KEY_PACKED_BITS_START };

/* byte order helpers */

fn high_word_offset(f: &bkey_format) -> u32 {
    if cfg!(target_endian = "little") { f.key_u64s as u32 - 1 } else { 0 }
}

fn nth_word(p: u32, n: u32) -> u32 {
    if cfg!(target_endian = "little") { p.wrapping_sub(n) } else { p.wrapping_add(n) }
}

fn next_word(p: u32) -> u32 {
    nth_word(p, 1)
}

fn prev_word(p: u32) -> u32 {
    if cfg!(target_endian = "little") { p.wrapping_add(1) } else { p.wrapping_sub(1) }
}

/// The walks over a key's words read the one past its last before they stop.
fn word(k: &[u64], p: u32) -> u64 {
    k.get(p as usize).copied().unwrap_or(0)
}

fn bkey_words(k: &c::bkey) -> [u64; BKEY_U64S] {
    let mut w = [0u64; BKEY_U64S];
    plain_bytes_mut(&mut w).copy_from_slice(plain_bytes(k));
    w
}

fn packed(k: &[u64]) -> &BkeyP {
    BkeyP::from_slice(k).expect("a packed key").cast()
}

fn set_pos_header(w: &mut u64, key_u64s: u8) {
    let h = Header::of_mut(w);
    h.u64s  = key_u64s;
    h.format_bits.set_format(c::KEY_FORMAT_LOCAL_BTREE as u8);
    h.type_ = c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
}

struct PackState<'a> {
    format: &'a bkey_format,
    k:      &'a mut [u64],
    bits:   u32,	/* bits remaining in current word */
    w:      u64,	/* current word */
    p:      u32,	/* next word */
}

impl<'a> PackState<'a> {
    #[inline(always)]
    fn new(format: &'a bkey_format, k: &'a mut [u64]) -> Self {
        Self { format, k, bits: 64 - HIGH_BIT_OFFSET, w: 0, p: high_word_offset(format) }
    }

    #[inline(always)]
    fn finish(self) {
        ebug_assert!(self.p < self.format.key_u64s as u32);

        self.k[self.p as usize] = self.w;
    }

    #[inline(always)]
    fn set_inc_field_unchecked(&mut self, f: bch_bkey_fields, v: u64) {
        let mut bits = self.format.bits(f);

        if bits != 0 {
            if bits > self.bits {
                bits -= self.bits;
                /* avoid shift by 64 if bits is 64 - bits is never 0 here: */
                self.w |= (v >> 1) >> (bits - 1);

                self.k[self.p as usize] = self.w;
                self.p = next_word(self.p);
                self.w = 0;
                self.bits = 64;
            }

            self.bits -= bits;
            self.w |= v << self.bits;
        }
    }

    #[inline(always)]
    fn set_inc_field(&mut self, f: bch_bkey_fields, v: u64) -> bool {
        let offset = self.format.offset(f);

        if v < offset {
            return false;
        }

        let v = v - offset;

        if fls64(v) > self.format.bits(f) {
            return false;
        }

        self.set_inc_field_unchecked(f, v);
        true
    }
}

struct UnpackState<'a> {
    format: &'a bkey_format,
    k:      &'a [u64],
    bits:   u32,	/* bits remaining in current word */
    w:      u64,	/* current word */
    p:      u32,	/* next word */
}

impl<'a> UnpackState<'a> {
    #[inline(always)]
    fn new(format: &'a bkey_format, k: &'a [u64]) -> Self {
        let p = high_word_offset(format);

        Self { format, k, bits: 64 - HIGH_BIT_OFFSET, w: word(k, p) << HIGH_BIT_OFFSET, p }
    }

    #[inline(always)]
    fn get_inc_field(&mut self, f: bch_bkey_fields) -> u64 {
        let mut bits = self.format.bits(f);
        let offset = self.format.offset(f);
        let mut v = 0;

        if bits >= self.bits {
            v = self.w >> (64 - bits);
            bits -= self.bits;

            self.p = next_word(self.p);
            self.w = word(self.k, self.p);
            self.bits = 64;
        }

        /* avoid shift by 64 if bits is 0 - bits is never 64 here: */
        v |= (self.w >> 1) >> (63 - bits);
        self.w <<= bits;
        self.bits -= bits;

        v.wrapping_add(offset)
    }
}

impl bkey_format {
    pub fn unpack_key(&self, k: &BkeyP<impl ?Sized>) -> c::bkey {
        self.unpack_key_words(k.as_slice())
    }

    fn unpack_key_words(&self, k: &[u64]) -> c::bkey {
        let h = Header::of(&k[0]);
        let mut s = UnpackState::new(self, k);

        ebug_assert!(self.nr_fields as usize == NR_FIELDS);
        ebug_assert!(h.u64s >= self.key_u64s);
        ebug_assert!(h.format_bits.format() as u32 == c::KEY_FORMAT_LOCAL_BTREE);
        ebug_assert!(h.u64s as u32 - self.key_u64s as u32 + BKEY_U64S as u32 <= u8::MAX as u32);

        let mut out = c::bkey {
            u64s:        (h.u64s as u32 - self.key_u64s as u32 + BKEY_U64S as u32) as u8,
            format_bits: c::bkey_format_bits::new()
                .with_format(c::KEY_FORMAT_CURRENT as u8)
                .with_needs_whiteout(h.format_bits.needs_whiteout()),
            type_:       h.type_,
            ..Default::default()
        };

        macro_rules! unpack_fields {
            ($( ($id:ident, $($field:ident).+) ),* $(,)?) => {
                $(out.$($field).+ = s.get_inc_field($id) as _;)*
            };
        }
        bkey_fields!(unpack_fields);
        out
    }

    pub fn unpack_pos(&self, k: &BkeyP<impl ?Sized>) -> Bpos {
        let mut s = UnpackState::new(self, k.as_slice());

        ebug_assert!(self.nr_fields as usize == NR_FIELDS);
        ebug_assert!(k.u64s() >= self.key_u64s as usize);
        ebug_assert!(k.format() as u32 == c::KEY_FORMAT_LOCAL_BTREE);

        let inode    = s.get_inc_field(BKEY_FIELD_INODE);
        let offset   = s.get_inc_field(BKEY_FIELD_OFFSET);
        let snapshot = s.get_inc_field(BKEY_FIELD_SNAPSHOT) as u32;
        Bpos { inode, offset, snapshot }
    }

    /// Just the key, not the value: false if it doesn't fit.
    pub fn pack_key(&self, out: &mut [u64], k: &c::bkey) -> bool {
        ebug_assert!(self.nr_fields as usize == NR_FIELDS);
        ebug_assert!(k.format_bits.format() as u32 == c::KEY_FORMAT_CURRENT);

        out[0] = 0;

        let mut s = PackState::new(self, out);

        macro_rules! pack_fields {
            ($( ($id:ident, $($field:ident).+) ),* $(,)?) => {
                $(if !s.set_inc_field($id, k.$($field).+ as u64) {
                    return false;
                })*
            };
        }
        bkey_fields!(pack_fields);
        s.finish();

        let h = Header::of_mut(&mut out[0]);
        h.u64s  = (self.key_u64s as u32 + k.u64s as u32 - BKEY_U64S as u32) as u8;
        h.format_bits.set_format(c::KEY_FORMAT_LOCAL_BTREE as u8);
        h.format_bits.set_needs_whiteout(k.format_bits.needs_whiteout());
        h.type_ = k.type_;

        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_bkey_unpack() {
            self.pack_verify(out, k);
        }
        true
    }

    #[cold]
    #[inline(never)]
    fn pack_verify(&self, packed: &[u64], unpacked: &c::bkey) {
        let h = Header::of(&packed[0]);

        assert!(h.u64s >= self.key_u64s, "packed to {} u64s, under its format's {}", h.u64s, self.key_u64s);
        assert!(h.u64s - self.key_u64s == unpacked.u64s - BKEY_U64S as u8,
                "packed to {} u64s of value, from {}", h.u64s - self.key_u64s, unpacked.u64s - BKEY_U64S as u8);

        let tmp = self.unpack_key_words(packed);

        if plain_bytes(&tmp) != plain_bytes(unpacked) {
            let mut buf = Printbuf::new();
            let b = &self.bits_per_field;

            write!(buf, "keys differ: format u64s {} fields {} {} {} {} {}\n",
                   self.key_u64s, b[0], b[1], b[2], b[3], b[4]);

            write!(buf, "compiled unpack: ");
            unpacked.to_text(&mut buf);
            write!(buf, "\n");

            write!(buf, "c unpack:        ");
            tmp.to_text(&mut buf);
            write!(buf, "\n");

            write!(buf, "compiled unpack: ");
            packed_to_binary_text(&mut buf, &BKEY_FORMAT_CURRENT, &bkey_words(unpacked));
            write!(buf, "\n");

            write!(buf, "c unpack:        ");
            packed_to_binary_text(&mut buf, &BKEY_FORMAT_CURRENT, &bkey_words(&tmp));
            write!(buf, "\n");

            panic!("{buf}");
        }
    }

    /// @p's fields, each past its offset: None if one doesn't fit.
    #[inline(always)]
    fn pos_fields(&self, p: Bpos) -> Option<[u64; 3]> {
        let mut v = [p.inode, p.offset, p.snapshot as u64];
        for (v, f) in v.iter_mut().zip(POS_FIELDS) {
            *v = v.wrapping_sub(self.offset(f));
        }

        /*
         * Single compare per field — folds underflow + overflow.
         * Relies on two invariants from bch2_bkey_format_field_overflows():
         *   bits == unpacked_bits  =>  field_offset == 0  (no underflow)
         *   field_offset + (2^bits - 1) <= unpacked_max   (underflow wraps
         *                                                  to fls64 == 64)
         * Bitwise | to keep this branchless.
         */
        let fail = v.iter().zip(POS_FIELDS).fold(false, |fail, (&v, f)| fail | (fls64(v) > self.bits(f)));

        if unlikely(fail) { None } else { Some(v) }
    }

    /*
     * The fields of the greatest position that packs and is <= @p, and whether
     * that's @p:
     *   - underflow (in.X < field_offset): roll up — bump the next-higher
     *     field and set this field (and all below) to MAX. Underflow on inode
     *     is FAIL.
     *   - overflow (delta > field_max): clamp this field to field_max and
     *     set all lower fields to MAX (cascading), since once a higher field
     *     has been rounded down, the only way to keep @out <= @in is to make
     *     everything below it MAX too.
     */
    #[inline(always)]
    fn pos_fields_lossy(&self, mut p: Bpos) -> Option<([u64; 3], bool)> {
        let mut exact = true;

        if unlikely((p.snapshot as u64) < self.offset(BKEY_FIELD_SNAPSHOT)) {
            p = spos(p.inode, p.offset, 0).checked_predecessor()?;
            exact = false;
        }

        if unlikely(p.offset < self.offset(BKEY_FIELD_OFFSET)) {
            p = spos(p.inode, 0, 0).checked_predecessor()?;
            exact = false;
        }

        if unlikely(p.inode < self.offset(BKEY_FIELD_INODE)) {
            return None;
        }

        let mut v = [p.inode, p.offset, p.snapshot as u64];
        for (v, f) in v.iter_mut().zip(POS_FIELDS) {
            *v -= self.offset(f);
        }

        /*
         * Overflow clamping with cascade: an overflowed field clamps
         * to its max and saturates the lower-order fields, giving the
         * largest packed key still <= the original pos.
         *
         * The cascaded fields' widths are independent of the branch
         * condition and can legitimately be 64 (e.g. offset in hash
         * btrees), hence u64_bitmask() and not (1ULL << bits) - 1.
         */
        if let Some(i) = v.iter().zip(POS_FIELDS).position(|(&v, f)| unlikely(fls64(v) > self.bits(f))) {
            for (v, &f) in v[i..].iter_mut().zip(&POS_FIELDS[i..]) {
                *v = u64_bitmask(self.bits(f));
            }
            exact = false;
        }

        Some((v, exact))
    }

    /// @v, a position's fields past their offsets, into @out: by the
    /// bit-stream state machine.
    fn pack_pos_fields(&self, out: &mut [u64], v: [u64; 3]) {
        /*
         * bch2_bkey_pack_key() will write to all of f->key_u64s, minus the 3
         * byte header, but pack_pos() won't if the len/version fields are big
         * enough - we need to make sure to zero them out:
         */
        out[..self.key_u64s as usize].fill(0);

        let mut s = PackState::new(self, out);
        for (f, v) in POS_FIELDS.into_iter().zip(v) {
            s.set_inc_field_unchecked(f, v);
        }
        s.finish();

        set_pos_header(&mut out[0], self.key_u64s);
    }

    /*
     * Most significant differing bit
     * Bits are indexed from 0 - return is [0, nr_key_bits)
     */
    pub fn greatest_differing_bit(&self, mut nr_key_bits: u32,
                                  l: &BkeyP<impl ?Sized>, r: &BkeyP<impl ?Sized>) -> u32 {
        let (l, r) = (l.as_slice(), r.as_slice());
        let mut p = high_word_offset(self);
        let mut word_bits = 64 - HIGH_BIT_OFFSET;

        ebug_assert!(nr_key_bits == self.key_bits());

        /* for big endian, skip past header */
        let mut l_v = word(l, p) & (!0u64 >> HIGH_BIT_OFFSET);
        let mut r_v = word(r, p) & (!0u64 >> HIGH_BIT_OFFSET);

        while nr_key_bits != 0 {
            if nr_key_bits < word_bits {
                l_v >>= word_bits - nr_key_bits;
                r_v >>= word_bits - nr_key_bits;
                nr_key_bits = 0;
            } else {
                nr_key_bits -= word_bits;
            }

            if l_v != r_v {
                return fls64(l_v ^ r_v) - 1 + nr_key_bits;
            }

            p = next_word(p);

            l_v = word(l, p);
            r_v = word(r, p);
            word_bits = 64;
        }

        0
    }

    #[inline(always)]
    pub(super) fn cmp_bits(&self, mut nr_key_bits: u32,
                           l: &BkeyP<impl ?Sized>, r: &BkeyP<impl ?Sized>) -> Ordering {
        let (l, r) = (l.as_slice(), r.as_slice());
        let mut p = high_word_offset(self);

        if nr_key_bits == 0 {
            return Ordering::Equal;
        }

        invariant!((p as usize) < l.len(), (p as usize) < r.len());

        /* for big endian, skip past header */
        nr_key_bits += HIGH_BIT_OFFSET;
        let mut l_v = l[p as usize] & (!0u64 >> HIGH_BIT_OFFSET);
        let mut r_v = r[p as usize] & (!0u64 >> HIGH_BIT_OFFSET);

        loop {
            if nr_key_bits < 64 {
                l_v >>= 64 - nr_key_bits;
                r_v >>= 64 - nr_key_bits;
                nr_key_bits = 0;
            } else {
                nr_key_bits -= 64;
            }

            if nr_key_bits == 0 || l_v != r_v {
                break;
            }

            p = next_word(p);
            invariant!((p as usize) < l.len(), (p as usize) < r.len());

            l_v = l[p as usize];
            r_v = r[p as usize];
        }

        l_v.cmp(&r_v)
    }
}

fn packed_to_binary_text(out: &mut Printbuf, f: &bkey_format, k: &[u64]) {
    let mut p = high_word_offset(f);
    let mut word_bits = 64 - HIGH_BIT_OFFSET;
    let mut nr_key_bits = f.key_bits() + HIGH_BIT_OFFSET;
    let mut v = word(k, p) & (!0u64 >> HIGH_BIT_OFFSET);

    if nr_key_bits == 0 {
        write!(out, "(empty)");
        return;
    }

    loop {
        let mut next_key_bits = nr_key_bits;

        if nr_key_bits < 64 {
            v >>= 64 - nr_key_bits;
            next_key_bits = 0;
        } else {
            next_key_bits -= 64;
        }

        write!(out, "{v:0width$b}", width = word_bits.min(nr_key_bits) as usize);

        if next_key_bits == 0 {
            break;
        }

        write!(out, " ");

        p = next_word(p);
        v = word(k, p);
        word_bits = 64;
        nr_key_bits = next_key_bits;
    }
}

/*
 * Note: does NOT set out->format (we don't know what it should be here!)
 *
 * Also: doesn't work on extents - it doesn't preserve the invariant that
 * if k is packed bkey_start_pos(k) will successfully pack
 */
fn transform_key(out_f: &bkey_format, out: &mut [u64], in_f: &bkey_format, k: &BkeyP<impl ?Sized>) -> bool {
    out[0] = 0;

    let mut out_s = PackState::new(out_f, out);
    let mut in_s = UnpackState::new(in_f, k.as_slice());

    for f in FIELDS {
        if !out_s.set_inc_field(f, in_s.get_inc_field(f)) {
            return false;
        }
    }

    /* Can't happen because the val would be too big to unpack: */
    ebug_assert!(k.u64s() - in_f.key_u64s as usize + out_f.key_u64s as usize <= u8::MAX as usize);

    out_s.finish();

    let o = Header::of_mut(&mut out[0]);
    o.u64s  = (out_f.key_u64s as usize + k.u64s() - in_f.key_u64s as usize) as u8;
    o.format_bits.set_needs_whiteout(k.needs_whiteout() as u8);
    o.type_ = k.key_type().0 as u8;
    true
}

pub fn transform(out_f: &bkey_format, out: &mut [u64], in_f: &bkey_format, k: &BkeyP<impl ?Sized>) -> bool {
    if !transform_key(out_f, out, in_f, k) {
        return false;
    }

    let val = &k.as_slice()[in_f.key_u64s as usize..];
    out[out_f.key_u64s as usize..][..val.len()].copy_from_slice(val);
    true
}

/*
 * The byte-aligned fast paths: a key is at @at in @buf, a node's buffer or a
 * bkey_packed_padded's, where the word before it is the buffer's too - the
 * loads start up to 8 bytes before the key.
 */

#[inline(always)]
fn le64_at(bytes: &[u8], at: u32) -> u64 {
    invariant!(at as usize + 8 <= bytes.len());

    u64::from_le_bytes(bytes[at as usize..][..8].try_into().unwrap())
}

/*
 * Pack-direction sibling of unpack_field_fast(): RMW into the same 8-byte
 * window the unpack constants describe, OR'ing @v in at the field's bit
 * position. Branchless even for the zero-width sentinel: @v is guaranteed 0
 * by the caller's range check when bits==0, so the OR is a no-op. The
 * `& 63` is to keep the shift defined when shift_right == 64.
 *
 * Correctness for negative byte_offset (lowest fields): the OR'd value has
 * zeros below the field's LSB bit, so RMW preserves whatever was in those
 * bytes (the byte(s) before @bytes, and the 3-byte bkey header).
 */
#[inline(always)]
fn pack_field_fast(bytes: &mut [u8], k: u32, uf: &BkeyUnpackField, v: u64) {
    let at = k.wrapping_add_signed(uf.byte_offset as i32);
    let raw = le64_at(bytes, at) | v << (uf.shift_right & 63);

    bytes[at as usize..][..8].copy_from_slice(&raw.to_le_bytes());
}

#[inline(always)]
fn unpack_field_fast(bytes: &[u8], k: u32, uf: &BkeyUnpackField, offset: u64) -> u64 {
    if uf.shift_right == 64 {
        return offset;
    }

    let raw = le64_at(bytes, k.wrapping_add_signed(uf.byte_offset as i32));

    /* Field ends at top of register; shr brings it to bit 0 cleanly */
    (raw >> uf.shift_right).wrapping_add(offset)
}

impl c::bkey_packed_padded {
    /// Its key, as the fast paths take one: a buffer, and its offset there.
    pub fn as_buf_mut(&mut self) -> (&mut [u64], u16) {
        let buf = cast_plain_slice_mut(plain_bytes_mut(self)).expect("a bkey_packed_padded is whole u64s");

        (buf, (offset_of!(Self, k) / size_of::<u64>()) as u16)
    }
}

impl NodeContents {
    /*
     * Precompute per-field unpack constants from b->format, stored in struct btree.
     *
     * For each field, picks byte_offset such that an 8-byte unaligned load ends
     * exactly at the byte after the field's MSB byte. The field lands in the
     * top of the loaded value, junk from earlier-in-memory fields lands in the
     * low bits. A single >> (64 - bits) then extracts the field cleanly.
     *
     * Only works when each field's MSB is at a byte boundary
     * (field_msb_bit % 8 == 7). bch2_bkey_format_done() rounds fields up to
     * byte width when there are spare bits, so this is the common case.
     * Formats too tight to byte-align fall back via byte_aligned_fields = false.
     */
    pub fn compute_unpack_consts(&mut self) {
        let format = self.format;
        let mut bit_offset = format.key_u64s as u32 * 64;

        if cfg!(target_endian = "big") {
            /* big-endian: not implemented yet, force fallback */
            self.byte_aligned_fields = false;
            return;
        }

        self.byte_aligned_fields = true;

        for (f, uf) in FIELDS.into_iter().zip(&mut self.unpack) {
            let bits = format.bits(f);

            uf.byte_offset = 0;
            uf.shift_right = 64;	/* sentinel: skip load, return offset */

            if bits == 0 {
                continue;
            }

            if bits > 64 {
                self.byte_aligned_fields = false;
                return;
            }

            bit_offset -= bits;
            let field_msb_bit = bit_offset + bits - 1;

            /* Need MSB at a byte boundary for single-shift extraction */
            if field_msb_bit % 8 != 7 {
                self.byte_aligned_fields = false;
                return;
            }

            /* Load 8 bytes ending at field's MSB byte + 1 */
            uf.byte_offset = ((field_msb_bit + 1) / 8) as i8 - 8;
            uf.shift_right = (64 - bits) as u8;
        }
    }

    /// Just the key at @at in @buf, not the value: by the node's unpack
    /// constants, if it's packed and the format's byte aligned.
    pub fn unpack_key_at(&self, buf: &[u64], at: u16) -> c::bkey {
        let k = packed(&buf[at as usize..]);

        if unlikely(!k.is_packed()) {
            return k.unpack(self);
        }

        /*
         * Header trick below relies on the packed key's format byte being
         * KEY_FORMAT_LOCAL_BTREE (0): the immediate add bumps it to
         * KEY_FORMAT_CURRENT (1). Also assert that the u64s addition can't
         * overflow byte 0 and corrupt the format byte (carry from u64s into
         * format would silently break things).
         */
        ebug_assert!(k.format() as u32 == c::KEY_FORMAT_LOCAL_BTREE);
        ebug_assert!(k.u64s() - self.format.key_u64s as usize + BKEY_U64S <= u8::MAX as usize);

        /* fall back to slow path if format has fields that don't fit a single load */
        if unlikely(!self.byte_aligned_fields) {
            return self.format.unpack_key(k);
        }

        let (bytes, kb) = (plain_bytes(buf), at as u32 * 8);
        invariant!(at >= 1, kb as usize + 8 <= bytes.len());

        /*
         * Header trick: the first 4 bytes of struct bkey are
         * { u64s, format/needs_whiteout, type, pad }. Load 4 bytes
         * starting one byte BEFORE @in — so the loaded bytes are
         * [junk, u64s, format, type]. A single >> 8 drops the junk,
         * positions u64s/format/type at bytes 0/1/2, and zero-fills
         * byte 3 (the pad). Then add an immediate to bump u64s by
         * (BKEY_U64s - key_u64s) and change format from LOCAL_BTREE
         * (0) to CURRENT (1).
         *
         * The carry from u64s into the next byte can't fire: BKEY_U64s
         * is small (<= 16) and key_u64s <= BKEY_U64s, so the addition
         * to byte 0 stays within byte 0.
         */
        let hdr = (u32::from_le_bytes(bytes[kb as usize - 1..][..4].try_into().unwrap()) >> 8)
            .wrapping_add((BKEY_U64S as u32).wrapping_sub(self.format.key_u64s as u32) |
                          ((c::KEY_FORMAT_CURRENT - c::KEY_FORMAT_LOCAL_BTREE) << 8));

        let mut out = c::bkey::default();
        plain_bytes_mut(&mut out)[..4].copy_from_slice(&hdr.to_le_bytes());

        macro_rules! unpack_fields {
            ($( ($id:ident, $($field:ident).+) ),* $(,)?) => {
                $(out.$($field).+ = unpack_field_fast(bytes, kb, &self.unpack[$id as usize],
                                                      self.format.offset($id)) as _;)*
            };
        }
        bkey_fields!(unpack_fields);

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            let check = self.format.unpack_key(k);

            assert!(plain_bytes(&out) == plain_bytes(&check),
                    "unpacked by the node's constants to {out}, by its format to {check}");
        }

        out
    }

    /*
     * Position-only sibling of unpack_key_at(): byte-aligned fast path
     * for the 3 bpos fields, avoiding the bit-stream state machine. Mirrors the
     * gate (byte_aligned_fields) and the precomputed @b->unpack[] constants.
     *
     * Profiling (sqlite-bench under FUSE, vtune): __bkey_unpack_pos was 7.5% of
     * total instructions retired via get_inc_field()'s ~25-insn-per-field state
     * machine. unpack_field_fast() is ~3 insns/field, so this trims ~70%+ of the
     * per-call cost on byte-aligned formats - which is the common case.
     */
    pub fn unpack_pos_at(&self, buf: &[u64], at: u16) -> Bpos {
        let k = packed(&buf[at as usize..]);

        if unlikely(!k.is_packed()) {
            return k.unpack_pos(self);
        }

        ebug_assert!(self.format.nr_fields as usize == NR_FIELDS);
        ebug_assert!(k.u64s() >= self.format.key_u64s as usize);
        ebug_assert!(k.format() as u32 == c::KEY_FORMAT_LOCAL_BTREE);

        if unlikely(!self.byte_aligned_fields) {
            return self.format.unpack_pos(k);
        }

        let (bytes, kb) = (plain_bytes(buf), at as u32 * 8);
        let field = |f: bch_bkey_fields| unpack_field_fast(bytes, kb, &self.unpack[f as usize], self.format.offset(f));

        let out = Bpos {
            inode:    field(BKEY_FIELD_INODE),
            offset:   field(BKEY_FIELD_OFFSET),
            snapshot: field(BKEY_FIELD_SNAPSHOT) as u32,
        };

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            let check = self.format.unpack_pos(k);

            assert!(out == check, "unpacked by the node's constants to {out}, by its format to {check}");
        }

        out
    }

    /*
     * Exact-pack: pack @p into the key at @at in @buf, false if it doesn't fit
     * the format.
     *
     * For byte-aligned formats (the common case after bch2_bkey_format_done()
     * rounds fields up to byte width), uses three pack_field_fast() RMWs
     * against the precomputed per-field load windows.
     */
    pub fn pack_pos(&self, buf: &mut [u64], at: u16, p: Bpos) -> bool {
        let Some(v) = self.format.pos_fields(p) else { return false };

        self.pack_pos_fields(buf, at, v);

        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_bkey_unpack() {
            self.pack_pos_verify(&buf[at as usize..], &p, true);
        }

        true
    }

    /*
     * Returns a packed key that compares <= in
     *
     * This is used in bset_search_tree(), where we need a packed pos in order to be
     * able to compare against the keys in the auxiliary search tree - and it's
     * legal to use a packed pos that isn't equivalent to the original pos,
     * _provided_ it compares <= to the original pos.
     *
     * Padding requirement is identical to bch2_bkey_pack_pos() — see
     * struct bkey_packed_padded.
     */
    pub fn pack_pos_lossy(&self, buf: &mut [u64], at: u16, p: Bpos) -> bkey_pack_pos_ret {
        let Some((v, exact)) = self.format.pos_fields_lossy(p) else {
            return bkey_pack_pos_ret::BKEY_PACK_POS_FAIL;
        };

        self.pack_pos_fields(buf, at, v);

        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_bkey_unpack() {
            self.pack_pos_verify(&buf[at as usize..], &p, exact);
        }

        if exact { bkey_pack_pos_ret::BKEY_PACK_POS_EXACT } else { bkey_pack_pos_ret::BKEY_PACK_POS_SMALLER }
    }

    /// @v, a position's fields past their offsets, into the key at @at in
    /// @buf: by the node's unpack constants if the format's byte aligned.
    #[inline(always)]
    fn pack_pos_fields(&self, buf: &mut [u64], at: u16, v: [u64; 3]) {
        let key_u64s = self.format.key_u64s as usize;

        if unlikely(!self.byte_aligned_fields) {
            return self.format.pack_pos_fields(&mut buf[at as usize..], v);
        }

        invariant!(at >= 1, key_u64s >= 1, at as usize + key_u64s <= buf.len());
        buf[at as usize..][..key_u64s].fill(0);

        let (bytes, kb) = (plain_bytes_mut(buf), at as u32 * 8);
        for (f, v) in POS_FIELDS.into_iter().zip(v) {
            pack_field_fast(bytes, kb, &self.unpack[f as usize], v);
        }

        set_pos_header(&mut buf[at as usize], self.format.key_u64s);

        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_bkey_unpack() {
            /* Packed key is always <= unpacked struct bkey in size */
            let mut check = [0u64; BKEY_U64S];
            self.format.pack_pos_fields(&mut check, v);

            assert!(buf[at as usize..][..key_u64s] == check[..key_u64s],
                    "packed by the node's constants to {}, by its format to {}",
                    packed(&buf[at as usize..]).unpack_pos(self), packed(&check).unpack_pos(self));
        }
    }

    /// @out, @orig packed: equal to it if @exact, else below it with no packed
    /// key between.
    #[cold]
    #[inline(never)]
    fn pack_pos_verify(&self, out: &[u64], orig: &Bpos, exact: bool) {
        let k = packed(out);
        let cmp = self.cmp_left_packed(k, orig);

        if exact {
            assert!(cmp == Ordering::Equal, "{orig} packed exactly, to {}", k.unpack_pos(self));
        } else {
            assert!(cmp == Ordering::Less, "{orig} packed lossily, to {}: not below it", k.unpack_pos(self));

            if let Some(successor) = self.packed_successor(out) {
                let successor = packed(&successor);

                assert!(self.cmp_left_packed(successor, orig) != Ordering::Less,
                        "{orig} packed lossily, to {}: {} packs, and is closer",
                        k.unpack_pos(self), successor.unpack_pos(self));
            }
        }
    }

    fn packed_successor(&self, k: &[u64]) -> Option<[u64; BKEY_U64S]> {
        let f = &self.format;
        let key_u64s = f.key_u64s as usize;
        let mut nr_key_bits = self.nr_key_bits as u32;

        ebug_assert!(nr_key_bits == f.key_bits());

        if nr_key_bits == 0 {
            return None;
        }

        let mut out = [0u64; BKEY_U64S];
        out[..key_u64s].copy_from_slice(&k[..key_u64s]);

        let first_bit = HIGH_BIT_OFFSET + nr_key_bits - 1;
        let mut p = nth_word(high_word_offset(f), first_bit >> 6);
        let mut offset = 63 - (first_bit & 63);

        while nr_key_bits != 0 {
            let bits = (64 - offset).min(nr_key_bits);
            let mask = (!0u64 >> (64 - bits)) << offset;

            if out[p as usize] & mask != mask {
                out[p as usize] += 1 << offset;
                ebug_assert!(self.cmp_packed(packed(&out), packed(k)) == Ordering::Greater);
                return Some(out);
            }

            out[p as usize] &= !mask;
            p = prev_word(p);
            nr_key_bits -= bits;
            offset = 0;
        }

        None
    }
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
pub fn pack_test() {
    let t = c::bkey { p: Bpos { inode: 4134, offset: 1250629070527416633, snapshot: 0 }, ..c::bkey::new() };
    let t_words = bkey_words(&t);
    let mut p = [0u64; BKEY_U64S];

    let test_format = bkey_format {
        key_u64s:       3,
        nr_fields:      BKEY_NR_FIELDS as u8,
        bits_per_field: [13, 64, 32, 0, 0, 0],
        ..Default::default()
    };

    let mut in_s = UnpackState::new(&BKEY_FORMAT_CURRENT, &t_words);
    let mut out_s = PackState::new(&test_format, &mut p);

    for f in FIELDS {
        let v = in_s.get_inc_field(f);

        macro_rules! field {
            ($( ($id:ident, $($field:ident).+) ),* $(,)?) => {
                match f {
                    $($id => t.$($field).+ as u64,)*
                    _ => unreachable!(),
                }
            };
        }
        let a = bkey_fields!(field);

        if a != v {
            panic!("got {v} actual {a} i {}", f as u32);
        }

        if !out_s.set_inc_field(f, v) {
            panic!("failed at {}", f as u32);
        }
    }

    assert!(test_format.pack_key(&mut p, &t));
}
