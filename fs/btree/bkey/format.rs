//! bkey_format, how a btree node packs its keys, and bkey_format_state, the
//! builder that picks one to fit the keys a node will hold.

use super::BKEY_U64S;
use crate::c;
use crate::cstructs::c::{bkey_fields, bkey_format_state};
use crate::debug::params::debug_check_bkey_unpack;
use crate::errcode::BchError;
use crate::format::bch_bkey_fields::{self, *};
use crate::format::{bkey_format, Bpos};
use crate::sb_err_on;
use crate::types::{field_size, Le64};
use crate::util::{Printbuf, ToText};

pub(super) const NR_FIELDS: usize = BKEY_NR_FIELDS as usize;

macro_rules! field_ids {
    ($( ($id:ident, $($field:ident).+) ),* $(,)?) => { [$($id),*] };
}

/// A key's fields, in the order they're packed.
pub(super) const FIELDS: [bch_bkey_fields; NR_FIELDS] = bkey_fields!(field_ids);

macro_rules! format_current {
    ($( ($id:ident, $($field:ident).+) ),* $(,)?) => {{
        let mut bits_per_field = [0; NR_FIELDS];
        $(bits_per_field[$id as usize] =
            (field_size(|k: *const c::bkey| unsafe { core::ptr::addr_of!((*k).$($field).+) }) * 8) as u8;)*

        bkey_format {
            key_u64s:     BKEY_U64S as u8,
            nr_fields:    BKEY_NR_FIELDS as u8,
            bits_per_field,
            field_offset: [Le64::new(0); NR_FIELDS],
        }
    }};
}

/// An unpacked key's: each field all of struct bkey's.
pub const BKEY_FORMAT_CURRENT: bkey_format = bkey_fields!(format_current);

pub(super) fn fls64(v: u64) -> u32 {
    u64::BITS - v.leading_zeros()
}

pub(super) fn u64_bitmask(bits: u32) -> u64 {
    if bits != 0 { !0u64 >> (64 - bits) } else { 0 }
}

impl bkey_format {
    pub fn bits(&self, f: bch_bkey_fields) -> u32 {
        self.bits_per_field[f as usize] as u32
    }

    pub fn offset(&self, f: bch_bkey_fields) -> u64 {
        self.field_offset[f as usize].get()
    }

    /*
     * We don't want it to be possible for the packed format to represent fields
     * bigger than a u64... that will cause confusion and issues (like with
     * bkey_packed_successor())
     */
    fn set_field(&mut self, f: bch_bkey_fields, bits: u32, offset: u64) {
        let unpacked_bits = BKEY_FORMAT_CURRENT.bits(f);
        let unpacked_max = u64_bitmask(unpacked_bits);

        let bits = bits.min(unpacked_bits);
        let offset = if bits == unpacked_bits { 0 } else { offset.min(unpacked_max - u64_bitmask(bits)) };

        self.bits_per_field[f as usize] = bits as u8;
        self.field_offset[f as usize]   = Le64::new(offset);
    }

    fn field_overflows(&self, f: bch_bkey_fields) -> bool {
        let f_bits = self.bits(f);
        let unpacked_bits = BKEY_FORMAT_CURRENT.bits(f);
        let field_offset = self.offset(f);

        if f_bits > unpacked_bits {
            return true;
        }

        if f_bits == unpacked_bits && field_offset != 0 {
            return true;
        }

        (field_offset.wrapping_add(u64_bitmask(f_bits)) & u64_bitmask(unpacked_bits)) < field_offset
    }

    pub fn validate(&self, err: &mut Printbuf) -> Result<(), BchError> {
        let mut bits = c::KEY_PACKED_BITS_START;

        sb_err_on!(err, self.nr_fields as usize != NR_FIELDS, BCH_ERR_invalid,
                   "incorrect number of fields: got {}, should be {NR_FIELDS}", self.nr_fields)?;

        /*
         * Verify that the packed format can't represent fields larger than the
         * unpacked format:
         */
        for f in FIELDS {
            sb_err_on!(err, self.field_overflows(f), BCH_ERR_invalid,
                       "field {} too large: {} + {} > {}", f as u32,
                       u64_bitmask(self.bits(f).min(64)), self.offset(f),
                       u64_bitmask(BKEY_FORMAT_CURRENT.bits(f)))?;

            bits += self.bits(f);
        }

        sb_err_on!(err, self.key_u64s as u32 != bits.div_ceil(64), BCH_ERR_invalid,
                   "incorrect key_u64s: got {}, should be {}", self.key_u64s, bits.div_ceil(64))
    }
}

impl ToText for bkey_format {
    fn to_text(&self, out: &mut Printbuf) {
        write!(out, "u64s {} fields ", self.key_u64s);

        for f in FIELDS {
            if f != BKEY_FIELD_INODE {
                write!(out, ", ");
            }
            write!(out, "{}:{}", self.bits(f), self.offset(f));
        }
    }
}
crate::display_via_to_text!(bkey_format);

impl bkey_format_state {
    pub fn new() -> Self {
        let mut s = Self { field_min: [u64::MAX; NR_FIELDS], field_max: [0; NR_FIELDS] };

        /* Make sure we can store a size of 0: */
        s.field_min[BKEY_FIELD_SIZE as usize] = 0;
        s
    }

    fn add(&mut self, f: bch_bkey_fields, v: u64) {
        self.field_min[f as usize] = self.field_min[f as usize].min(v);
        self.field_max[f as usize] = self.field_max[f as usize].max(v);
    }

    pub fn add_pos(&mut self, p: Bpos) {
        self.add(BKEY_FIELD_INODE,    p.inode);
        self.add(BKEY_FIELD_OFFSET,   p.offset);
        self.add(BKEY_FIELD_SNAPSHOT, p.snapshot as u64);
    }

    /*
     * Changes @format so that @k can be successfully packed with @format
     */
    pub fn add_key(&mut self, k: &c::bkey) {
        macro_rules! add {
            ($( ($id:ident, $($field:ident).+) ),* $(,)?) => { $(self.add($id, k.$($field).+ as u64);)* };
        }
        bkey_fields!(add);
    }

    pub fn done(&mut self) -> bkey_format {
        let mut bits = c::KEY_PACKED_BITS_START;
        let mut ret = bkey_format { nr_fields: BKEY_NR_FIELDS as u8, ..Default::default() };

        for f in FIELDS {
            let (min, max) = (&mut self.field_min[f as usize], self.field_max[f as usize]);
            *min = (*min).min(max);

            ret.set_field(f, fls64(max - *min), *min);

            bits += ret.bits(f);
        }

        ret.key_u64s = bits.div_ceil(64) as u8;

        /* if we have enough spare bits, round fields up to nearest byte */
        let mut bits = ret.key_u64s as u32 * 64 - bits;

        for f in FIELDS {
            let r = ret.bits(f).next_multiple_of(8) - ret.bits(f);

            if r <= bits {
                ret.set_field(f, ret.bits(f) + r, ret.offset(f));
                bits -= r;
            }
        }

        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_bkey_unpack() {
            let mut buf = Printbuf::new();
            assert!(ret.validate(&mut buf).is_ok(), "bkey_format_state::done() made an invalid format {ret}: {buf}");
        }

        ret
    }
}
