// SPDX-License-Identifier: GPL-2.0
//
// Superblock display with device names — Rust replacement for the C
// bch2_sb_to_text_with_names().
//
// The C version called bch2_scan_device_sbs (Rust FFI) which returned
// Vec-allocated memory via forget(), then freed it with darray_exit
// (kvfree) — allocator mismatch causing heap corruption. This version
// keeps everything in Rust so the Vec is dropped with the correct
// allocator.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

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

/// Print a concise one-line-per-member summary: device index, path (or
/// "(not found)" if no matching device was scanned), state, error
/// counters, and a short issue tag ("ok" if none apply).
///
/// Reuses the same UUID-based device-path discovery as
/// sb_to_text_with_names(), retaining the supplied member path even when
/// discovery cannot enumerate it. No online filesystem is required.
pub fn sb_members_summary_to_text(out: &mut Printbuf, sb: &c::bch_sb, device_path: &Path) -> anyhow::Result<()> {
    let uuid = uuid::Uuid::from_bytes(sb.user_uuid.b);
    let device_str = format!("UUID={}", uuid);

    let opts = bcachefs_kernel::opts::parse_mount_opts(None, None, true).unwrap_or_default();
    let sbs = device_scan::scan_sbs(&device_str, &opts)?;

    out.aligned(|sub| {
        write!(sub, "Device\tPath\tState\tRead err\tWrite err\tChecksum err\tIssues\r\n");

        let mut print_member = |idx: u32, m: c::bch_member, has_errors: bool| {
            if !m.alive() {
                return;
            }

            let path = if idx == sb.dev_idx as u32 {
                Some(device_path.to_string_lossy().into_owned())
            } else {
                find_dev(&sbs, idx).map(|(path, _)| path.to_string_lossy().into_owned())
            };
            let missing = path.is_none();
            let path = path.unwrap_or_else(|| "(not found)".to_string());

            let state = m.state();
            // members_v1 entries predate the error counters: unavailable, not zero
            let (err_read, err_write, err_checksum, err_flush) = if has_errors {
                (m.errors[0].get(), m.errors[1].get(), m.errors[2].get(), m.flush_errors.get())
            } else {
                (0, 0, 0, 0)
            };

            let mut issues = Vec::new();
            if missing {
                issues.push("missing".to_string());
            }
            if state != c::MemberState::rw {
                issues.push(format!("state={state}"));
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
            if err_flush != 0 {
                issues.push(format!("flush_errors={err_flush}"));
            }
            let issues = match (issues.is_empty(), has_errors) {
                (true, true) => "ok".to_string(),
                (true, false) => "no error counters (members_v1)".to_string(),
                _ => issues.join(","),
            };
            let counter = |v: u64| if has_errors { v.to_string() } else { "n/a".to_string() };

            write!(
                sub, "{idx}\t{path}\t{state}\t{}\t{}\t{}\t{issues}\r\n",
                counter(err_read), counter(err_write), counter(err_checksum)
            );
        };

        if let Some(members) = sb::members::members_v2(sb) {
            for idx in 0..members.nr_devices() {
                if let Some(m) = members.get(idx) {
                    print_member(idx, m, true);
                }
            }
        } else if let Some(members) = sb::members::members_v1(sb) {
            for idx in 0..members.nr_devices() {
                if let Some(m) = members.get(idx) {
                    print_member(idx, m, false);
                }
            }
        }
    });
    Ok(())
}
