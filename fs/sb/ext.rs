// SPDX-License-Identifier: GPL-2.0

//! The superblock's ext section: what recovery is to do on the next mount -
//! passes to run, errors to fix silently, btrees that lost data - and since
//! it grew, what's been learned of the btrees over the filesystem's life.

use core::mem::{offset_of, size_of};

use crate::c;
use crate::fs::Fs;
use crate::init::passes;
use crate::sb::errors::prt_error_ids;
use crate::sb::io::SbFieldOps;
use crate::types::Le64;
use crate::util::bitmap::LeBitmap;
use crate::util::vstructs::{Validated, Unvalidated};
use crate::util::Printbuf;

type Section = c::bch_sb_field_ext<Validated>;

fn prt_btrees(out: &mut Printbuf, what: &str, btrees: Le64) {
    write!(out, "{what}:\t");
    // SAFETY: the names are C's, NULL terminated
    unsafe { out.prt_bitflags(c::__bch2_btree_ids.as_ptr(), btrees.get()) };
    out.newline();
}

/// Whether @e has the field it grew at @off: only if a version that knew of
/// it wrote the section.
fn has(e: &Section, off: usize) -> bool {
    e.as_bytes().len() >= off + size_of::<Le64>()
}

/// Its validate is the arm's: a section shorter than the 88 bytes of the
/// fields from before it grew isn't one.
impl SbFieldOps for Section {
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                e: &c::bch_sb_field_ext<Unvalidated>) {
        write!(out, "Recovery passes required:\t");
        passes::prt_stable(out, e.recovery_passes_required[0].get());
        out.newline();

        write!(out, "Errors to silently fix:\t");
        let silent = LeBitmap::from_words(&e.errors_silent).iter();
        prt_error_ids(out, silent.take_while(|&id| id < c::BCH_FSCK_ERR_MAX as usize)
                                 .map(|id| c::bch_sb_error_id(id as u32)));
        out.newline();

        prt_btrees(out, "Btrees with missing data", e.btrees_lost_data());

        if has(e, offset_of!(c::bch_sb_field_ext__whole, btrees_lost_data_ever)) {
            prt_btrees(out, "Btrees that have ever lost data", e.btrees_lost_data_ever());
        }
        if has(e, offset_of!(c::bch_sb_field_ext__whole, btrees_clean)) {
            prt_btrees(out, "Btrees validated clean", e.btrees_clean());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::types::btree_id_str;
    use crate::types::Le32;
    use crate::util::vstructs::{cast_plain_mut, plain_bytes_mut, Head, Unvalidated, VStruct};

    /// An ext section of @u64s, @buf's first, as the superblock's printer
    /// prints it - by its type.
    fn to_text(buf: &mut [u64], u64s: usize) -> String {
        // a superblock of no sections: its header
        let mut sb = [0u64; size_of::<Head<c::bch_sb>>() / size_of::<u64>()];
        let sb_bytes = plain_bytes_mut(&mut sb[..]);
        cast_plain_mut::<Head<c::bch_sb>>(sb_bytes).unwrap().set_val_u64s(0);
        let sb = c::bch_sb::<Unvalidated>::from_bytes(sb_bytes).unwrap();

        let b = plain_bytes_mut(buf);
        let f = cast_plain_mut::<Head<c::bch_sb_field>>(b).unwrap();
        f.set_val_u64s(u64s);
        f.type_ = Le32::new(c::bch_sb_field_type::BCH_SB_FIELD_ext.0);
        let f = c::bch_sb_field::<Unvalidated>::from_bytes(b).unwrap();

        let mut out = Printbuf::new();
        crate::sb::io::field_ops_to_text(&mut out, None, sb, f);
        out.as_str().to_owned()
    }

    /// The fields it grew are printed only if it has them: 88 bytes has
    /// neither, 112 btrees_lost_data_ever, 120 btrees_clean too.
    #[test]
    fn to_text_grown_fields() {
        let mut buf = [0u64; 15];
        buf[3] = 1 << 2 | 1;                    // errors_silent: ids 0 and 2
        buf[13] = 1;                            // btrees_lost_data_ever: btree 0
        buf[14] = 1;                            // btrees_clean: btree 0

        let names: Vec<_> = [0, 2].map(|id| c::bch_sb_error_id(id).name().unwrap()).into();
        let lines = |u64s| to_text(&mut buf.clone(), u64s).lines().map(str::to_owned).collect::<Vec<_>>();

        let l = lines(11);
        assert_eq!(l.len(), 3);
        assert!(l[1].ends_with(&names.join(",")), "{}", l[1]);

        let l = lines(14);
        assert_eq!(l.len(), 4);
        assert!(l[3].starts_with("Btrees that have ever lost data") && l[3].ends_with(&btree_id_str(0)));

        let l = lines(15);
        assert_eq!(l.len(), 5);
        assert!(l[4].starts_with("Btrees validated clean") && l[4].ends_with(&btree_id_str(0)));
    }
}
