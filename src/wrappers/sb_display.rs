// SPDX-License-Identifier: GPL-2.0
//
// Superblock display with device names.

use std::os::fd::AsRawFd;
use std::path::PathBuf;

use bcachefs_kernel::format::Sb;
use bcachefs_kernel::sb::{members_format::Member, self, SbFieldType, io::SbState};
use bcachefs_kernel::util::{printbuf::Printbuf, vstructs::Unvalidated};
use bcachefs_kernel::init::dev::SbHandle;
use bcachefs_kernel::fs::Fs;

use crate::device_scan;

/// Find a scanned device by its superblock dev_idx.
fn find_dev(sbs: &[(PathBuf, SbHandle)], idx: u32) -> Option<&(PathBuf, SbHandle)> {
    sbs.iter().find(|(_, sb_handle)| sb_handle.sb().dev_idx as u32 == idx)
}

/// Print one member device's info: name, model, and detailed member text.
fn print_one_member(
    out: &mut Printbuf,
    sbs: &[(PathBuf, SbHandle)],
    sb: &Sb,
    m: &Member,
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
        let fd = sb_handle.bdev().fd().as_raw_fd();
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
    sb: &Sb,
    print_layout: bool,
    fields: u32,
    field_only: Option<SbFieldType>,
) {
    // Build UUID= device string for scanning
    let uuid = uuid::Uuid::from_bytes(sb.user_uuid.b);
    let device_str = format!("UUID={}", uuid);

    let opts = bcachefs_kernel::opts::parse_mount_opts(None, None, true).unwrap_or_default();
    let sbs = device_scan::scan_sbs(&device_str, &opts).unwrap_or_default();

    let sb_as_read = sb.as_unvalidated();

    if let Some(ty) = field_only {
        if let Some(f) = Unvalidated::field_by_type(sb_as_read, ty) {
            sb::io::field_ops_to_text(out, fs, sb_as_read, f);
        }
    } else {
        out.tabstop_push(44);

        let member_mask = SbFieldType::members_v1.bit()
            | SbFieldType::members_v2.bit();
        sb::io::sb_to_text(out, fs, sb_as_read, print_layout, fields & !member_mask);

        // members_v1
        if (fields & SbFieldType::members_v1.bit()) != 0 {
            if let Some(mi1) = sb::members::members_v1(sb) {
                for i in 0..mi1.nr_devices() {
                    if let Some(m) = mi1.get(i) {
                        print_one_member(out, &sbs, sb, &m, i);
                    }
                }
            }
        }

        // members_v2
        if (fields & SbFieldType::members_v2.bit()) != 0 {
            if let Some(mi2) = sb::members::members_v2(sb) {
                for i in 0..mi2.nr_devices() {
                    if let Some(m) = mi2.get(i) {
                        print_one_member(out, &sbs, sb, &m, i);
                    }
                }
            }
        }
    }

    // sbs (Vec<(PathBuf, SbHandle)>) is dropped here — freed by
    // Rust's allocator, not kvfree. This is the whole point.
}
