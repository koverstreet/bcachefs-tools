// SPDX-License-Identifier: GPL-2.0

//! The extent_type_u64s superblock section: each extent entry type's size,
//! so a version that doesn't know a newer type can still step over one. The
//! in-memory copy is c->sb.extent_type_u64s, a fixed size array: the
//! section's sizes, then ours over the types we know.

use crate::c;
use crate::data::ptrs::{extent_entry_type_name, extent_entry_type_u64s};
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::sb::io::{set_all, SbFieldOps};
use crate::util::ffi::Opaque;
use crate::util::vstructs::Head;
use crate::util::Printbuf;
use core::ffi::c_int;
use cstruct_macros::c_export;

fn ours() -> impl Iterator<Item = u8> {
    (0..c::BCH_EXTENT_ENTRY_MAX).map(|ty| extent_entry_type_u64s(ty).unwrap() as u8)
}

/// A 0 ends the section's sizes.
fn sizes(e: &c::bch_sb_field_extent_type_u64s) -> &[u8] {
    &e.d[..e.d.iter().position(|&u64s| u64s == 0).unwrap_or(e.d.len())]
}

/// The table c->sb holds of @theirs - as many as it holds, ours over the
/// types we know - and how many types it knows.
fn sizes_to_cpu(theirs: &[u8]) -> ([u8; 31], u8) {
    let mut u64s = [0; 31];
    let nr = theirs.len().min(u64s.len());

    u64s[..nr].copy_from_slice(&theirs[..nr]);
    for (d, s) in u64s.iter_mut().zip(ours()) {
        *d = s;
    }
    (u64s, nr.max(c::BCH_EXTENT_ENTRY_MAX as usize) as u8)
}

/// c->sb's extent entry sizes, from the superblock's section.
pub(crate) fn sb_extent_type_u64s_to_cpu(sb: &mut SbLockGuard) {
    let (handle, cpu) = sb.parts();
    let theirs = handle.field::<c::bch_sb_field_extent_type_u64s>().map_or(&[][..], sizes);
    let (u64s, known) = sizes_to_cpu(theirs);

    set_all(&cpu.extent_type_u64s, u64s);
    cpu.extent_types_known.set(known);
}

fn sb_extent_type_u64s_from_cpu(fs: &Fs, sb: &mut SbLockGuard) -> Result<(), BchError> {
    let u64s = (size_of::<Head<c::bch_sb_field>>() + c::BCH_EXTENT_ENTRY_MAX as usize)
        .div_ceil(size_of::<u64>());
    let e = sb.field_get_minsize::<c::bch_sb_field_extent_type_u64s>(u64s as u32);
    let Some(e) = e else {
        crate::bch_err!(fs, "error allocating superblock space for extent_type_u64s");
        return fs.throw(bch_errcode::BCH_ERR_ENOSPC_sb_extent_type_u64s);
    };

    for (d, s) in e.d.iter_mut().zip(ours()) {
        *d = s;
    }
    Ok(())
}

#[c_export]
pub fn bch2_sb_extent_type_u64s_from_cpu(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    ret_to_c(sb_extent_type_u64s_from_cpu(fs, &mut unsafe { fs.sb_lock_held() }))
}

impl SbFieldOps for c::bch_sb_field_extent_type_u64s {
    fn validate(_sb: &c::bch_sb, e: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
        for (ty, (&theirs, ours)) in sizes(e).iter().zip(ours()).enumerate() {
            if theirs != ours {
                write!(err, "extent_type_u64s for {} does not match in-mem ({theirs} != {ours})",
                       extent_entry_type_name(ty as u32).unwrap());
                return Err(bch_errcode::BCH_ERR_invalid_sb_extent_type_u64s.into());
            }
        }
        Ok(())
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, e: &Self) {
        for (ty, &u64s) in sizes(e).iter().enumerate() {
            match extent_entry_type_name(ty as u32) {
                Some(name) => write!(out, "{name}"),
                None       => write!(out, "(unknown type {ty})"),
            }
            write!(out, ":\t{u64s}\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Le32;
    use crate::util::vstructs::{VStruct, Validated};

    fn section<'a>(buf: &'a mut [u64], sizes: &[u8]) -> &'a c::bch_sb_field_extent_type_u64s {
        let f = buf.as_mut_ptr().cast::<Head<c::bch_sb_field>>();
        // SAFETY: u64s, which any bytes are, aligned for a section, as many
        // as its header says; @sizes fit past the header
        unsafe {
            (*f).set_val_u64s(buf.len());
            (*f).type_ = Le32::new(c::bch_sb_field_type::BCH_SB_FIELD_extent_type_u64s.0);
            core::slice::from_raw_parts_mut(f.add(1).cast::<u8>(), sizes.len()).copy_from_slice(sizes);
            c::bch_sb_field::<Validated>::from_ffi(f).arm().unwrap()
        }
    }

    /// An older version's section has fewer types, a newer one's more: ours
    /// win for the types we know, theirs are kept past them.
    #[test]
    fn to_cpu_ours_over_theirs() {
        let ours: Vec<u8> = ours().collect();

        let (u64s, known) = sizes_to_cpu(&[9, 9, 9]);
        assert_eq!(u64s[..ours.len()], ours[..]);
        assert_eq!(known, c::BCH_EXTENT_ENTRY_MAX as u8);

        let newer = [&ours[..], &[4, 5]].concat();
        let (u64s, known) = sizes_to_cpu(&newer);
        assert_eq!(u64s[..newer.len()], newer[..]);
        assert_eq!(usize::from(known), newer.len());
    }

    /// No more than the table holds, however long the section: C wrote past it.
    #[test]
    fn to_cpu_bounded() {
        let (u64s, known) = sizes_to_cpu(&[1; 64]);
        assert_eq!(usize::from(known), u64s.len());
    }

    #[test]
    fn validate_sizes() {
        let sb = [0u64; 32];
        // SAFETY: a superblock header of no sections
        let sb = unsafe { c::bch_sb::<Validated>::from_ffi(sb.as_ptr().cast()) };
        let flags = c::bch_validate_flags::from_bits_retain(0);
        let validate = |sizes: &[u8]| {
            let mut buf = [0u64; 4];
            let mut err = Printbuf::new();
            <c::bch_sb_field_extent_type_u64s as SbFieldOps>::validate(sb, section(&mut buf, sizes), flags, &mut err)
                .map_err(|_| err.as_str().to_owned())
        };

        let mut sizes: Vec<u8> = ours().collect();
        assert_eq!(validate(&sizes), Ok(()));
        sizes.extend([7, 7]);
        assert_eq!(validate(&sizes), Ok(()));

        sizes[0] = 2;
        assert_eq!(validate(&sizes).unwrap_err(), "extent_type_u64s for ptr does not match in-mem (2 != 1)");
    }
}
