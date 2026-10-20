// SPDX-License-Identifier: GPL-2.0
//
// Superblock display with device names — Rust replacement for the C
// bch2_sb_to_text_with_names() in rust_shims.c.
//
// The C version called bch2_scan_device_sbs (Rust FFI) which returned
// Vec-allocated memory via forget(), then freed it with darray_exit
// (kvfree) — allocator mismatch causing heap corruption. This version
// keeps everything in Rust so the Vec is dropped with the correct
// allocator.

use std::path::PathBuf;

use bch_bindgen::c;
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::c::bch_sb_handle;
use bcachefs_kernel::sb::{self, sb_field_type};
use bcachefs_kernel::util::vstructs::{Head, Validated};

use crate::device_scan;

/// Find a scanned device by its superblock dev_idx.
fn find_dev(sbs: &[(PathBuf, bch_sb_handle)], idx: u32) -> Option<&(PathBuf, bch_sb_handle)> {
    sbs.iter().find(|(_, sb_handle)| sb_handle.sb().dev_idx as u32 == idx)
}

/// Print one member device's info: name, model, and detailed member text.
fn print_one_member(
    out: &mut Printbuf,
    sbs: &[(PathBuf, bch_sb_handle)],
    sb: &c::bch_sb,
    m: &c::bch_member,
    idx: u32,
) {
    if !m.alive() {
        return;
    }

    let dev = find_dev(sbs, idx);
    let name_str = dev
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(not found)".to_string());

    write!(out, "Device {}:\t{}\t", idx, name_str);

    if let Some((_, sb_handle)) = dev {
        use crate::wrappers::bdev;
        let fd = sb_handle.bdev().bd_fd;
        let model = bdev::fd_to_dev_model(fd);
        write!(out, "{}", model);
        if let Some(serial) = bdev::fd_to_dev_serial(fd) {
            write!(out, "\tS/N: {}", serial);
        }
    }
    out.newline();

    sb::members::member_to_text(&mut out.indent(2), m, sb, idx);
}

/// Print superblock contents with device names.
///
/// Scans for devices matching the superblock's UUID, then prints
/// superblock fields and per-member details with device paths and
/// hardware model names.
///
/// # Safety
/// `fs` must be a valid pointer to a `bch_fs` or null.
/// `sb` must point to a valid `bch_sb`.
pub unsafe fn sb_to_text_with_names(
    out: &mut Printbuf,
    fs: *mut c::bch_fs,
    sb: &c::bch_sb,
    print_layout: bool,
    fields: u32,
    field_only: i32,
) {
    // Build UUID= device string for scanning
    let uuid = uuid::Uuid::from_bytes(sb.user_uuid.b);
    let device_str = format!("UUID={}", uuid);

    let opts = bcachefs_kernel::opts::parse_mount_opts(None, None, true).unwrap_or_default();
    let sbs = device_scan::scan_sbs(&device_str, &opts).unwrap_or_default();

    let sb_ptr = sb.as_ffi() as *mut Head<c::bch_sb>;

    let fs_ref = (!fs.is_null()).then(|| bcachefs_kernel::fs::Fs::borrow_raw(fs));

    if field_only >= 0 {
        let f = sb::io::bch2_sb_field_get_id(sb_ptr, c::bch_sb_field_type(field_only as u32));
        if !f.is_null() {
            sb::io::field_ops_to_text(out, fs_ref, sb, c::bch_sb_field::<Validated>::from_ffi(f));
        }
    } else {
        out.tabstop_push(44);

        let member_mask = sb_field_type::members_v1.bit()
            | sb_field_type::members_v2.bit();
        sb::io::sb_to_text(out, fs_ref, sb, print_layout, fields & !member_mask);

        // members_v1
        if (fields & sb_field_type::members_v1.bit()) != 0 {
            if let Some(mi1) = sb::members::members_v1(sb) {
                for i in 0..mi1.nr_devices() {
                    if let Some(m) = mi1.get(i) {
                        print_one_member(out, &sbs, sb, &m, i);
                    }
                }
            }
        }

        // members_v2
        if (fields & sb_field_type::members_v2.bit()) != 0 {
            if let Some(mi2) = sb::members::members_v2(sb) {
                for i in 0..mi2.nr_devices() {
                    if let Some(m) = mi2.get(i) {
                        print_one_member(out, &sbs, sb, &m, i);
                    }
                }
            }
        }
    }

    // sbs (Vec<(PathBuf, bch_sb_handle)>) is dropped here — freed by
    // Rust's allocator, not kvfree. This is the whole point.
}
