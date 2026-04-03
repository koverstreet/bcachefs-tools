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

use std::fmt::Write;
use std::path::PathBuf;

use bch_bindgen::c;
use bch_bindgen::printbuf::Printbuf;
use bch_bindgen::bcachefs::bch_sb_handle;
use bch_bindgen::sb;

use crate::device_scan;

/// UUID of a deleted member slot — all 0xff except the variant/clock_seq bytes.
const BCH_SB_MEMBER_DELETED_UUID: [u8; 16] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

/// Check if a member slot is alive (has a real device, not empty or deleted).
fn member_alive(m: &c::bch_member) -> bool {
    let zero = [0u8; 16];
    m.uuid.b != zero && m.uuid.b != BCH_SB_MEMBER_DELETED_UUID
}

/// Find a scanned device by its superblock dev_idx.
fn find_dev(sbs: &[(PathBuf, bch_sb_handle)], idx: u32) -> Option<&(PathBuf, bch_sb_handle)> {
    sbs.iter().find(|(_, sb_handle)| sb_handle.sb().dev_idx as u32 == idx)
}

/// Print one member device's info: name, model, and detailed member text.
///
/// # Safety
/// `sb` and `gi` must be valid pointers (gi may be null).
unsafe fn print_one_member(
    out: &mut Printbuf,
    sbs: &[(PathBuf, bch_sb_handle)],
    sb: *mut c::bch_sb,
    gi: *mut c::bch_sb_field_disk_groups,
    m: &mut c::bch_member,
    idx: u32,
) {
    if !member_alive(m) {
        return;
    }

    let dev = find_dev(sbs, idx);
    let name_str = dev
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(not found)".to_string());

    write!(out, "Device {}:\t{}\t", idx, name_str).unwrap();

    if let Some((_, sb_handle)) = dev {
        use crate::wrappers::bdev;
        let fd = sb_handle.bdev().bd_fd;
        let model = bdev::fd_to_dev_model(fd);
        write!(out, "{}", model).unwrap();
        if let Some(serial) = bdev::fd_to_dev_serial(fd) {
            write!(out, "\tS/N: {}", serial).unwrap();
        }
    }
    out.newline();

    {
        let mut indented = out.indent(2);
        c::bch2_member_to_text(indented.as_raw(), m, gi, sb, idx);
    }
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

    let opts = bch_bindgen::opts::parse_mount_opts(None, None, true).unwrap_or_default();
    let sbs = device_scan::scan_sbs(&device_str, &opts).unwrap_or_default();

    let sb_ptr = sb as *const c::bch_sb as *mut c::bch_sb;

    if field_only >= 0 {
        let f = c::bch2_sb_field_get_id(sb_ptr, std::mem::transmute::<u32, c::bch_sb_field_type>(field_only as u32));
        if !f.is_null() {
            c::__bch2_sb_field_to_text(out.as_raw(), fs, sb_ptr, f);
        }
    } else {
        out.tabstop_push(44);

        let member_mask = (1u32 << c::bch_sb_field_type::BCH_SB_FIELD_members_v1 as u32)
            | (1u32 << c::bch_sb_field_type::BCH_SB_FIELD_members_v2 as u32);
        c::bch2_sb_to_text(out.as_raw(), fs, sb_ptr, print_layout, fields & !member_mask);

        let gi: *mut c::bch_sb_field_disk_groups =
            sb::sb_field_get::<c::bch_sb_field_disk_groups>(sb)
                .map(|f| f as *const _ as *mut _)
                .unwrap_or(std::ptr::null_mut());

        // members_v1
        if (fields & (1 << c::bch_sb_field_type::BCH_SB_FIELD_members_v1 as u32)) != 0 {
            if let Some(mi1) = sb::members_v1(sb) {
                for i in 0..mi1.nr_devices() {
                    if let Some(mut m) = mi1.get(i) {
                        print_one_member(out, &sbs, sb_ptr, gi, &mut m, i);
                    }
                }
            }
        }

        // members_v2
        if (fields & (1 << c::bch_sb_field_type::BCH_SB_FIELD_members_v2 as u32)) != 0 {
            if let Some(mi2) = sb::members_v2(sb) {
                for i in 0..mi2.nr_devices() {
                    if let Some(mut m) = mi2.get(i) {
                        print_one_member(out, &sbs, sb_ptr, gi, &mut m, i);
                    }
                }
            }
        }
    }

    // sbs (Vec<(PathBuf, bch_sb_handle)>) is dropped here — freed by
    // Rust's allocator, not kvfree. This is the whole point.
}
