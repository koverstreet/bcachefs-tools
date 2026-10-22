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
use std::path::{Path, PathBuf};

use bch_bindgen::c;
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::c::bch_sb_handle;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::sb::{self, sb_field_type};
use bcachefs_kernel::sb::io::SbState;
use bcachefs_kernel::util::vstructs::Unvalidated;

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
/// hardware model names - or, with @field_only, that section alone.
pub fn sb_to_text_with_names(
    out: &mut Printbuf,
    fs: Option<&Fs>,
    sb: &c::bch_sb,
    print_layout: bool,
    fields: u32,
    field_only: Option<sb_field_type>,
) {
    // Build UUID= device string for scanning
    let uuid = uuid::Uuid::from_bytes(sb.user_uuid.b);
    let device_str = format!("UUID={}", uuid);

    let opts = bcachefs_kernel::opts::parse_mount_opts(None, None, true).unwrap_or_default();
    let sbs = device_scan::scan_sbs(&device_str, &opts).unwrap_or_default();

    let sb_as_read = sb.as_unvalidated();

    if let Some(ty) = field_only {
        if let Some(f) = Unvalidated::field(sb_as_read, ty) {
            sb::io::field_ops_to_text(out, fs, sb_as_read, f);
        }
    } else {
        out.tabstop_push(44);

        let member_mask = sb_field_type::members_v1.bit()
            | sb_field_type::members_v2.bit();
        sb::io::sb_to_text(out, fs, sb_as_read, print_layout, fields & !member_mask);

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

/// Print a concise one-line-per-member summary: device index, path (or
/// "(not found)" if no matching device was scanned), state, error
/// counters, and a short issue tag ("ok" if none apply).
///
/// Reuses the same UUID-based device-path discovery as
/// sb_to_text_with_names(), retaining the supplied member path even when
/// discovery cannot enumerate it. No online filesystem is required.
///
/// # Safety
/// `sb` must point to a valid `bch_sb`.
pub unsafe fn sb_members_summary_to_text(out: &mut Printbuf, sb: &c::bch_sb, device_path: &Path) {
    let uuid = uuid::Uuid::from_bytes(sb.user_uuid.b);
    let device_str = format!("UUID={}", uuid);

    let opts = bcachefs_kernel::opts::parse_mount_opts(None, None, true).unwrap_or_default();
    let sbs = device_scan::scan_sbs(&device_str, &opts).unwrap_or_default();

    out.aligned(|sub| {
        write!(sub, "Device\tPath\tState\tRead err\tWrite err\tChecksum err\tIssues\r\n");

        let mut print_member = |idx: u32, m: c::bch_member| {
            if !member_alive(&m) {
                return;
            }

            let path = if idx == sb.dev_idx as u32 {
                Some(device_path.to_string_lossy().into_owned())
            } else {
                find_dev(&sbs, idx).map(|(path, _)| path.to_string_lossy().into_owned())
            };
            let missing = path.is_none();
            let path = path.unwrap_or_else(|| "(not found)".to_string());

            let state = m.state() as u8;
            let state_str = sb::members::member_state_str(state);
            let err_read = m.errors[0].get();
            let err_write = m.errors[1].get();
            let err_checksum = m.errors[2].get();

            let mut issues = Vec::new();
            if missing {
                issues.push("missing".to_string());
            }
            if state != c::bch_member_state::BCH_MEMBER_STATE_rw.0 as u8 {
                issues.push(format!("state={state_str}"));
            }
            if err_read != 0 {
                issues.push(format!("read_errors={err_read}"));
            }
            if err_write != 0 {
                issues.push(format!("write_errors={err_write}"));
            }
            if err_checksum != 0 {
                issues.push(format!("checksum_errors={err_checksum}"));
            }
            let issues = if issues.is_empty() { "ok".to_string() } else { issues.join(",") };

            write!(
                sub, "{idx}\t{path}\t{state_str}\t{err_read}\t{err_write}\t{err_checksum}\t{issues}\r\n"
            );
        };

        if let Some(members) = sb::members::members_v2(sb) {
            for idx in 0..members.nr_devices() {
                if let Some(m) = members.get(idx) {
                    print_member(idx, m);
                }
            }
        } else if let Some(members) = sb::members::members_v1(sb) {
            for idx in 0..members.nr_devices() {
                if let Some(m) = members.get(idx) {
                    print_member(idx, m);
                }
            }
        }
    });
}
