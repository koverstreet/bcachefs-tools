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

use std::ffi::CStr;
use std::fmt::Write;
use std::path::PathBuf;

use bch_bindgen::c;
use bch_bindgen::printbuf::Printbuf;
use bch_bindgen::bcachefs::bch_sb_handle;

use crate::device_scan;

const BCH_MEMBER_V1_BYTES: usize = 56;

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

/// Read a bch_member from a members_v2 field at index `i`.
///
/// Handles variable member_bytes: copies min(member_bytes, sizeof(bch_member))
/// into a zeroed bch_member.
///
/// # Safety
/// `mi` must point to a valid bch_sb_field_members_v2 with at least `i+1`
/// member entries.
unsafe fn members_v2_get(mi: &c::bch_sb_field_members_v2, i: u32) -> c::bch_member {
    let member_bytes = u16::from_le(mi.member_bytes) as usize;
    let base = mi._members.as_ptr() as *const u8;
    let src = base.add(i as usize * member_bytes);
    let mut ret: c::bch_member = std::mem::zeroed();
    let copy_len = member_bytes.min(std::mem::size_of::<c::bch_member>());
    std::ptr::copy_nonoverlapping(src, &mut ret as *mut _ as *mut u8, copy_len);
    ret
}

/// Read a bch_member from a members_v1 field at index `i`.
///
/// V1 members have a fixed 56-byte stride.
///
/// # Safety
/// `mi` must point to a valid bch_sb_field_members_v1 with at least `i+1`
/// member entries.
unsafe fn members_v1_get(mi: &c::bch_sb_field_members_v1, i: u32) -> c::bch_member {
    let base = mi._members.as_ptr() as *const u8;
    let src = base.add(i as usize * BCH_MEMBER_V1_BYTES);
    let mut ret: c::bch_member = std::mem::zeroed();
    let copy_len = BCH_MEMBER_V1_BYTES.min(std::mem::size_of::<c::bch_member>());
    std::ptr::copy_nonoverlapping(src, &mut ret as *mut _ as *mut u8, copy_len);
    ret
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
        let model = c::fd_to_dev_model(sb_handle.bdev().bd_fd);
        if !model.is_null() {
            let model_str = CStr::from_ptr(model).to_string_lossy();
            write!(out, "{}", model_str).unwrap();
            libc::free(model as *mut _);
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

        let gi = c::bch2_sb_field_get_id(sb_ptr, c::bch_sb_field_type::BCH_SB_FIELD_disk_groups)
            as *mut c::bch_sb_field_disk_groups;

        // members_v1
        if (fields & (1 << c::bch_sb_field_type::BCH_SB_FIELD_members_v1 as u32)) != 0 {
            let mi1 = c::bch2_sb_field_get_id(sb_ptr, c::bch_sb_field_type::BCH_SB_FIELD_members_v1)
                as *const c::bch_sb_field_members_v1;
            if !mi1.is_null() {
                for i in 0..sb.nr_devices as u32 {
                    let mut m = members_v1_get(&*mi1, i);
                    print_one_member(out, &sbs, sb_ptr, gi, &mut m, i);
                }
            }
        }

        // members_v2
        if (fields & (1 << c::bch_sb_field_type::BCH_SB_FIELD_members_v2 as u32)) != 0 {
            let mi2 = c::bch2_sb_field_get_id(sb_ptr, c::bch_sb_field_type::BCH_SB_FIELD_members_v2)
                as *const c::bch_sb_field_members_v2;
            if !mi2.is_null() {
                for i in 0..sb.nr_devices as u32 {
                    let mut m = members_v2_get(&*mi2, i);
                    print_one_member(out, &sbs, sb_ptr, gi, &mut m, i);
                }
            }
        }
    }

    // sbs (Vec<(PathBuf, bch_sb_handle)>) is dropped here — freed by
    // Rust's allocator, not kvfree. This is the whole point.
}
