// SPDX-License-Identifier: GPL-2.0

//! Disk groups: a tree of device labels, named by dotted paths ("ssd.fast")
//! in targets and options. c->disk_groups, the in-memory table, is rebuilt
//! from the superblock section under sb_lock and read under RCU.
//!
//! A group's parent is its index + 1, 0 for a root. Deleted groups stay in
//! place, so indices are stable.
//!
//! The target lookups on hot paths - target_decode(), target_rw_devs(),
//! bch2_dev_in_target() - stay static inlines in disk_groups.h.

/* DOC_LATEX(failure-domains)
 * A \emph{failure domain} is a set of devices expected to fail together: a
 * shared controller, host, or rack. Spreading a filesystem's replicas across
 * failure domains is what lets it survive one failing --- losing a whole
 * domain then costs at most one replica of any extent.
 *
 * Each device may be given a failure domain: a free-form string, set with the
 * \texttt{failure\_domain} device option at format time, at device add time,
 * or at runtime. Two devices are in the same failure domain when their strings
 * are equal; a device with no failure domain set is a domain of its own.
 *
 * This is a flat, intrinsic device property with no relationship to the disk
 * label tree used for \hyperref[sec:io-path-options]{targeting}: a device's
 * label positions it for target options, its failure domain describes physical
 * fault isolation, and the two need not agree --- devices labeled by
 * performance class (\texttt{ssd}, \texttt{hdd}) may sit in racks that cut
 * across those classes.
 *
 * \subsubsection{Spreading replicas}
 *
 * When allocating, candidate devices are sorted by how many replicas of the
 * current write already sit in each device's failure domain, so each replica
 * goes to the least occupied domain, ties broken by the usual free space round
 * robin. The journal spreads its replicas the same way.
 *
 * For replicated data this is a preference, not a requirement: if every
 * remaining domain already holds a replica --- too few domains for the
 * replication level, or the empty ones are full --- replicas double up rather
 * than failing the allocation. Data sharing a domain with another of its
 * replicas is still intact; it just isn't protected against that domain's loss.
 *
 * \subsubsection{Erasure coding}
 *
 * For erasure coding, distinct failure domains are a hard requirement rather
 * than a preference: a stripe survives losing a domain only if no more than
 * \texttt{nr\_redundant} of its blocks are in it, and a stripe that loses too
 * many blocks cannot be reconstructed --- landing poorly is not merely
 * unprotected, as for replicated data, but unrecoverable. So stripe block
 * allocation excludes devices sharing an already-placed block's domain, and
 * stripe width is capped at the number of failure domains with allocatable
 * devices: if a domain becomes unavailable, new stripes are allocated narrower
 * rather than doubling up.
 *
 * \subsubsection{Checking failure domains}
 *
 * \texttt{bcachefs fs failure-domains} shows the failure domains and what
 * losing each would cost, computed from the replicas accounting: for each
 * domain, the data that would be lost if it failed --- too few copies or stripe
 * blocks left to reconstruct --- and the data that would survive degraded.
 * Nonzero lost data means failure domain separation is being violated. With no
 * failure domains configured each device is its own domain, so this becomes the
 * per device view.
 *
 * Existing data is not rewritten when failure domains change: the report shows
 * what a future rewrite pass would need to move.
 */

use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::sb::io::{sb_field_get, sb_field_ops, SbFieldOps};
use crate::sb::members;
use crate::util::ffi::{c_char, Opaque};
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::vstructs::{Head, Validated};
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint, CStr};
use cstruct_macros::c_export;

// ── Targets ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Null,
    Dev(u32),
    Group(u32),
}

impl Target {
    pub fn decode(target: u32) -> Self {
        if target >= c::TARGET_GROUP_START {
            Target::Group(target - c::TARGET_GROUP_START)
        } else if target >= c::TARGET_DEV_START {
            Target::Dev(target - c::TARGET_DEV_START)
        } else {
            Target::Null
        }
    }

    pub fn encode(self) -> u16 {
        (match self {
            Target::Null     => 0,
            Target::Dev(d)   => c::TARGET_DEV_START + d,
            Target::Group(g) => c::TARGET_GROUP_START + g,
        }) as u16
    }
}

// ── Groups, and their paths ──────────────────────────────────────────────

const DISK_PATH_MAX: usize = 32;

/// NUL terminated only if shorter than the field.
fn label_bytes(label: &[u8; c::BCH_SB_LABEL_SIZE as usize]) -> &[u8] {
    let len = label.iter().position(|&b| b == 0).unwrap_or(label.len());
    &label[..len]
}

/// A group in the superblock section or in c->disk_groups.
trait DiskGroup {
    fn deleted(&self) -> bool;
    /// Its parent's index plus one; 0 for a root.
    fn parent(&self) -> u32;
    fn label(&self) -> &[u8];
}

impl DiskGroup for c::bch_disk_group {
    fn deleted(&self) -> bool { self.bch_group_deleted() }
    fn parent(&self)  -> u32  { self.bch_group_parent() as u32 }
    fn label(&self)   -> &[u8] { label_bytes(&self.label) }
}

impl DiskGroup for c::bch_disk_group_cpu {
    fn deleted(&self) -> bool { self.deleted }
    fn parent(&self)  -> u32  { self.parent.into() }
    fn label(&self)   -> &[u8] { label_bytes(&self.label) }
}

/// "invalid label N" if the walk up hits a missing or deleted group (N), or
/// nests too deep.
fn disk_path_to_text_in<G: DiskGroup>(out: &mut Printbuf, groups: &[G], mut v: u32) {
    let mut path = [0u16; DISK_PATH_MAX];
    let mut nr = 0;

    loop {
        let e = groups.get(v as usize).filter(|e| !e.deleted());
        let Some(e) = e.filter(|_| nr < path.len()) else {
            write!(out, "invalid label {v}");
            return;
        };

        path[nr] = v as u16;
        nr += 1;

        if e.parent() == 0 {
            break;
        }
        v = e.parent() - 1;
    }

    for (i, &g) in path[..nr].iter().rev().enumerate() {
        out.write_bytes(groups[g as usize].label());
        if i + 1 < nr {
            write!(out, ".");
        }
    }
}

fn sb_groups(sb: &c::bch_sb) -> &[c::bch_disk_group] {
    sb_field_get::<c::bch_sb_field_disk_groups>(sb).map_or(&[], |g| &g.entries)
}

/// "ssd.fast." is "ssd.fast".
fn path_labels(name: &[u8]) -> impl Iterator<Item = &[u8]> {
    name.strip_suffix(b".").unwrap_or(name).split(|&b| b == b'.')
}

fn label_valid(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= c::BCH_SB_LABEL_SIZE as usize
}

fn group_find(groups: &[c::bch_disk_group], parent: u32, name: &[u8]) -> Option<u32> {
    if !label_valid(name) {
        return None;
    }
    groups.iter()
        .position(|g| !g.deleted() && g.parent() == parent && g.label() == name)
        .map(|i| i as u32)
}

/// Reuses the first deleted slot, else grows the section.
fn group_add(sb: &mut c::bch_sb_handle, parent: u32, name: &[u8]) -> Result<u32, BchError> {
    if !label_valid(name) {
        return Err(c::EINVAL.into());
    }

    let groups = sb_groups(sb.sb());
    let nr = groups.len();
    let i = groups.iter().position(|g| g.deleted()).unwrap_or(nr);

    if i == nr {
        let u64s = (size_of::<Head<c::bch_sb_field>>() +
                    size_of::<c::bch_disk_group>() * (nr + 1)) / size_of::<u64>();

        sb.field_resize::<c::bch_sb_field_disk_groups>(u64s as u32)
            .ok_or(BchError::from(bch_errcode::BCH_ERR_ENOSPC_disk_label_add))?;
    }

    let groups = sb.field_mut::<c::bch_sb_field_disk_groups>()
        .expect("disk groups section: had a deleted group, or was just sized");
    let g = &mut groups.entries[i];

    g.label = Default::default();
    g.label[..name.len()].copy_from_slice(name);
    g.set_bch_group_deleted(false);
    g.set_bch_group_parent(parent.into());
    g.set_bch_group_data_allowed(c::bch_disk_group::BCH_GROUP_DATA_ALLOWED_MAX);

    Ok(i as u32)
}

impl c::bch_sb_handle {
    pub fn disk_path_find(&self, name: &[u8]) -> Option<u32> {
        let groups = sb_groups(self.sb());
        // index + 1: path_labels() yields at least one label
        let mut v = 0;
        for l in path_labels(name) {
            v = group_find(groups, v, l)? + 1;
        }
        Some(v - 1)
    }

    /// An error leaves the ancestors added before it.
    pub fn disk_path_find_or_create(&mut self, name: &[u8]) -> Result<u32, BchError> {
        let mut v = 0;
        for l in path_labels(name) {
            let g = match group_find(sb_groups(self.sb()), v, l) {
                Some(g) => g,
                None    => group_add(self, v, l)?,
            };
            v = g + 1;
        }
        Ok(v - 1)
    }
}

impl Fs {
    /// Takes its own rcu read lock, @out held atomic.
    pub fn disk_path_to_text(&self, out: &mut Printbuf, v: u32) {
        disk_path_to_text(out, Some(self), v);
    }

    fn group_devs<'g>(&'g self, g: u32, rcu: &'g ReadGuard) -> Option<&'g c::bch_devs_mask> {
        self.disk_groups.read(rcu).get(g as usize).filter(|e| !e.deleted).map(|e| &e.devs)
    }

    pub fn target_to_mask<'g>(&'g self, target: Target, rcu: &'g ReadGuard) -> Option<&'g c::bch_devs_mask> {
        match target {
            Target::Null     => None,
            Target::Dev(d)   => self.dev_rcu(d, rcu).map(|ca| &ca.self_mask),
            Target::Group(g) => self.group_devs(g, rcu),
        }
    }

    pub fn dev_in_target(&self, dev: u32, target: Target, rcu: &ReadGuard) -> bool {
        if dev == c::BCH_SB_MEMBER_INVALID {
            return false;
        }

        match target {
            Target::Null     => false,
            Target::Dev(d)   => dev == d,
            Target::Group(g) => self.group_devs(g, rcu).is_some_and(|m| m.d.test(dev as usize)),
        }
    }

    pub fn target_to_text(&self, out: &mut Printbuf, target: Target) {
        match target {
            Target::Null => write!(out, "none"),
            Target::Dev(d) => {
                let mut out = out.atomic();
                let rcu = read_lock();

                match self.dev_rcu(d, &rcu) {
                    Some(ca) if !ca.disk_sb.bdev.is_null() => {
                        write!(out, "/dev/");
                        out.write_bytes(ca.name().to_bytes());
                    }
                    Some(_) => write!(out, "offline device {d}"),
                    None    => write!(out, "invalid device {d}"),
                }
            }
            Target::Group(g) => self.disk_path_to_text(out, g),
        }
    }

    /// 0: no failure domain, or no such device.
    fn dev_failure_domain(&self, dev: usize, rcu: &ReadGuard) -> u16 {
        self.dev_rcu(dev as u32, rcu).map_or(0, |ca| ca.mi.failure_domain)
    }
}

fn disk_path_to_text(out: &mut Printbuf, fs: Option<&Fs>, v: u32) {
    let mut out = out.atomic();
    let rcu = read_lock();
    let groups = fs.map_or(&[][..], |fs| fs.disk_groups.read(&rcu));

    disk_path_to_text_in(&mut out, groups, v);
}

fn target_to_text_sb(out: &mut Printbuf, sb: &c::bch_sb, target: Target) {
    match target {
        Target::Null => write!(out, "none"),
        Target::Dev(d) => {
            let m = members::sb_member(sb, d).filter(|m| d < u32::from(sb.nr_devices) && m.alive());
            match m {
                Some(m) => write!(out, "Device {} ({d})", uuid::Uuid::from_bytes(m.uuid.b)),
                None    => write!(out, "Bad device {d}"),
            }
        }
        Target::Group(g) => disk_path_to_text_in(out, sb_groups(sb), g),
    }
}

// ── The table ────────────────────────────────────────────────────────────

/// Without a disk groups section the table is left as it is.
fn sb_disk_groups_to_cpu(fs: &Fs) -> Result<(), BchError> {
    fs.assert_sb_lock_held();

    let sb = fs.disk_sb();
    let Some(groups) = sb.field::<c::bch_sb_field_disk_groups>() else {
        return Ok(());
    };
    let src = &groups.entries;

    let fill = |dst: &mut [c::bch_disk_group_cpu]| {
        for (d, s) in dst.iter_mut().zip(src.iter()) {
            d.deleted = s.bch_group_deleted();
            d.parent  = s.bch_group_parent() as u16;
            d.label   = s.label;
        }

        for i in 0..sb.sb().nr_devices as u32 {
            let m = members::sb_member(sb.sb(), i);
            let Some(m) = m.filter(|m| m.alive()) else { continue };

            let mut g = m.group() as usize;
            for _ in 0..dst.len() {
                let Some(d) = g.checked_sub(1).and_then(|g| dst.get_mut(g)) else { break };
                d.devs.d.set(i as usize);
                g = d.parent as usize;
            }
        }
    };

    // SAFETY: under sb_lock, the table's writers'
    unsafe { fs.disk_groups.rebuild(src.len(), fill) }
        .map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_disk_groups_to_cpu))
}

#[c_export]
pub fn bch2_sb_disk_groups_to_cpu(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(sb_disk_groups_to_cpu(Fs::from_c(c)))
}

/// # Safety
/// No readers or writers are left.
#[c_export]
pub unsafe fn bch2_fs_disk_groups_exit(c: &Opaque<c::bch_fs>) {
    // SAFETY: the caller's
    unsafe { Fs::from_c(c).disk_groups.free() }
}

fn dev_group_set(fs: &Fs, dev: u32, name: &[u8]) -> Result<(), BchError> {
    fs.assert_sb_lock_held();

    let v = dev_label_resolve(fs, name)?;
    // SAFETY: under sb_lock
    unsafe { fs.member_mut(dev) }.set_group(v.into());
    sb_disk_groups_to_cpu(fs)
}

/// As a member's group field: index + 1, 0 for none. Caller holds sb_lock.
fn dev_label_resolve(fs: &Fs, name: &[u8]) -> Result<u32, BchError> {
    if name.is_empty() || name == b"none" {
        return Ok(0);
    }
    // SAFETY: under sb_lock
    Ok(unsafe { fs.disk_sb_mut() }.disk_path_find_or_create(name)? + 1)
}

/// # Safety
/// @name is a C string.
#[c_export]
pub unsafe fn __bch2_dev_group_set(c: &Opaque<c::bch_fs>, ca: &c::bch_dev, name: *const c_char) -> c_int {
    // SAFETY: the caller's
    let name = unsafe { CStr::from_ptr(name.cast()) };
    ret_to_c(dev_group_set(Fs::from_c(c), ca.dev_idx.into(), name.to_bytes()))
}

/// @c may be NULL: every label is invalid then.
#[cold]
#[c_export]
pub fn bch2_disk_path_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>, v: c_uint) {
    disk_path_to_text(out, c.map(Fs::from_c), v);
}

/// # Safety
/// @sb is a superblock that's been validated.
#[cold]
#[c_export]
pub unsafe fn bch2_disk_path_to_text_sb(out: &mut Printbuf, sb: *mut Head<c::bch_sb>, v: c_uint) {
    // SAFETY: the caller's
    let sb = unsafe { c::bch_sb::<Validated>::from_ffi(sb) };
    disk_path_to_text_in(out, sb_groups(sb), v);
}

/// sysfs disk_groups.
#[cold]
#[c_export]
pub fn bch2_disk_groups_to_text(out: &mut Printbuf, c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);

    out.make_room_gfp(4096, c::GFP_KERNEL);

    let mut out = out.atomic();
    let rcu = read_lock();
    let groups = fs.disk_groups.read(&rcu);

    for (i, e) in groups.iter().enumerate() {
        write!(out, "{i:2}: ");

        if !e.deleted {
            disk_path_to_text_in(&mut out, groups, i as u32);

            write!(out, " devs");
            for ca in fs.devs_rcu_in(e.devs.d.iter(), &rcu) {
                write!(out, " ");
                out.write_bytes(ca.name().to_bytes());
            }
        } else {
            write!(out, "[deleted]");
        }

        out.newline();
    }
}

// ── Lookups, for C ───────────────────────────────────────────────────────
//
// C calls these holding rcu: the nested read lock each takes is so Rust can
// borrow - what's returned is good for the caller's read side section.

#[c_export]
pub fn bch2_target_to_mask(c: &Opaque<c::bch_fs>, target: c_uint) -> *const c::bch_devs_mask {
    let rcu = read_lock();
    Fs::from_c(c).target_to_mask(Target::decode(target), &rcu).map_or(core::ptr::null(), |m| m)
}

#[c_export]
pub fn bch2_dev_in_target_rcu(c: &Opaque<c::bch_fs>, dev: c_uint, target: c_uint) -> bool {
    Fs::from_c(c).dev_in_target(dev, Target::decode(target), &read_lock())
}

/// Spreading key for a candidate device: the number of already-chosen
/// replicas in its failure domain. Sorting candidates by this ascending fills
/// empty domains first, then doubles up - the failure-domain preference for
/// replicated data. A device with no failure domain (id 0) always keys 0: it
/// never shares a domain, so it sorts first and is never excluded.
#[c_export]
pub fn bch2_dev_domain_key(c: &Opaque<c::bch_fs>, chosen: &c::bch_devs_mask, dev: c_uint) -> u64 {
    let fs = Fs::from_c(c);
    let rcu = read_lock();

    match fs.dev_failure_domain(dev as usize, &rcu) {
        0 => 0,
        d => chosen.d.iter().filter(|&i| fs.dev_failure_domain(i, &rcu) == d).count() as u64,
    }
}

/// Number of distinct failure domains @devs spans: each set domain counted
/// once, at its first device, and each device with no domain as its own.
/// This caps stripe width when failure domains are a hard requirement - a
/// stripe can't have more blocks than domains to spread them across.
#[c_export]
pub fn bch2_target_nr_domains(c: &Opaque<c::bch_fs>, devs: &c::bch_devs_mask) -> c_uint {
    let fs = Fs::from_c(c);
    let rcu = read_lock();
    let domain = |dev| fs.dev_failure_domain(dev, &rcu);

    let nr = devs.d.iter()
        .filter(|&i| match domain(i) {
            0 => true,
            d => !devs.d.iter().take_while(|&j| j < i).any(|j| domain(j) == d),
        })
        .count();
    nr as c_uint
}

#[cold]
#[c_export]
pub fn bch2_target_to_text(out: &mut Printbuf, c: &Opaque<c::bch_fs>, v: c_uint) {
    Fs::from_c(c).target_to_text(out, Target::decode(v));
}

// ── Options: target, and disk_label ──────────────────────────────────────

fn opt_target_parse(fs: &Fs, val: &[u8]) -> Result<Target, BchError> {
    if val.is_empty() || val == b"none" {
        return Ok(Target::Null);
    }

    // as bch2_dev_lookup(): by name, with or without /dev/
    let name = val.strip_prefix(b"/dev/").unwrap_or(val);
    let dev = {
        let rcu = read_lock();
        let dev = fs.devs_rcu(&rcu).find(|ca| ca.name().to_bytes() == name).map(|ca| ca.dev_idx);
        dev
    };
    if let Some(dev) = dev {
        return Ok(Target::Dev(dev.into()));
    }

    let g = {
        let _lock = fs.sb_lock();
        fs.disk_sb().disk_path_find(val)
    };
    match g {
        Some(g) => Ok(Target::Group(g)),
        None    => fs.throw(bch_errcode::BCH_ERR_EINVAL_opt_target_parse_not_found),
    }
}

/// # Safety
/// @val is a C string, or NULL.
unsafe fn opt_parse_args<'a>(c: Option<&'a Opaque<c::bch_fs>>, val: *const c_char)
    -> Result<(&'a Fs, &'a [u8]), c_int>
{
    if val.is_null() {
        return Err(-(c::EINVAL as c_int));
    }
    let Some(c) = c else {
        return Err(-(bch_errcode::BCH_ERR_option_needs_open_fs.0 as c_int));
    };
    // SAFETY: the caller's
    Ok((Fs::from_c(c), unsafe { CStr::from_ptr(val.cast()) }.to_bytes()))
}

/// # Safety
/// @val is a C string, or NULL.
#[c_export]
pub unsafe fn bch2_opt_target_parse(c: Option<&Opaque<c::bch_fs>>, val: *const c_char,
                                    res: &mut u64, _err: Option<&mut Printbuf>) -> c_int {
    // SAFETY: the caller's
    let (fs, val) = match unsafe { opt_parse_args(c, val) } {
        Ok(a)  => a,
        Err(e) => return e,
    };
    ret_to_c(opt_target_parse(fs, val).map(|t| *res = t.encode().into()))
}

/// # Safety
/// @sb is a validated superblock, if @c is NULL.
#[cold]
#[c_export]
pub unsafe fn bch2_opt_target_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>,
                                      sb: *mut Head<c::bch_sb>, v: u64) {
    match c {
        Some(c) => Fs::from_c(c).target_to_text(out, Target::decode(v as u32)),
        // SAFETY: the caller's
        None    => target_to_text_sb(out, unsafe { c::bch_sb::<Validated>::from_ffi(sb) }, Target::decode(v as u32)),
    }
}

/// # Safety
/// @val is a C string, or NULL.
#[c_export]
pub unsafe fn bch2_opt_disk_label_parse(c: Option<&Opaque<c::bch_fs>>, val: *const c_char,
                                        res: &mut u64, _err: Option<&mut Printbuf>) -> c_int {
    // SAFETY: the caller's
    let (fs, val) = match unsafe { opt_parse_args(c, val) } {
        Ok(a)  => a,
        Err(e) => return e,
    };
    let _lock = fs.sb_lock();
    ret_to_c(dev_label_resolve(fs, val).map(|v| *res = v.into()))
}

/// # Safety
/// @sb is a validated superblock, if @c is NULL.
#[cold]
#[c_export]
pub unsafe fn bch2_opt_disk_label_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>,
                                          sb: *mut Head<c::bch_sb>, v: u64) {
    let Some(v) = (v as u32).checked_sub(1) else {
        write!(out, "none");
        return;
    };
    match c {
        Some(c) => Fs::from_c(c).disk_path_to_text(out, v),
        // SAFETY: the caller's
        None    => disk_path_to_text_in(out, sb_groups(unsafe { c::bch_sb::<Validated>::from_ffi(sb) }), v),
    }
}

// ── The superblock section ───────────────────────────────────────────────

fn invalid() -> Result<(), BchError> {
    Err(bch_errcode::BCH_ERR_invalid_sb_disk_groups.into())
}

/// Parents are walked, so must be in range; a deleted parent is allowed.
fn disk_groups_validate(sb: &c::bch_sb, groups: &[c::bch_disk_group], err: &mut Printbuf)
    -> Result<(), BchError>
{
    let nr = groups.len();

    for i in 0..u32::from(sb.nr_devices) {
        let Some(m) = members::sb_member(sb, i) else { continue };
        let Some(g) = (m.group() as usize).checked_sub(1) else { continue };

        match groups.get(g) {
            None => {
                write!(err, "disk {i} has invalid label {g} (have {nr})");
                return invalid();
            }
            Some(e) if e.deleted() => {
                write!(err, "disk {i} has deleted label {g}");
                return invalid();
            }
            Some(_) => {}
        }
    }

    let live = || groups.iter().enumerate().filter(|(_, g)| !g.deleted());

    for (i, g) in live() {
        if g.label().is_empty() {
            write!(err, "label {i} empty");
            return invalid();
        }
    }

    for (i, g) in live() {
        if g.parent() as usize > nr {
            write!(err, "label {i} has invalid parent {} (have {nr})", g.parent() - 1);
            return invalid();
        }
    }

    for (i, g) in live() {
        if live().any(|(j, h)| j > i && h.parent() == g.parent() && h.label() == g.label()) {
            write!(err, "duplicate label {}.", g.parent());
            err.write_bytes(g.label());
            return invalid();
        }
    }

    Ok(())
}

impl SbFieldOps for c::bch_sb_field_disk_groups {
    fn validate(sb: &c::bch_sb, f: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
        disk_groups_validate(sb, &f.entries, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        for (i, g) in f.entries.iter().enumerate() {
            if i > 0 {
                write!(out, " ");
            }

            if g.deleted() {
                write!(out, "[deleted]");
            } else {
                write!(out, "[parent {} name ", g.parent());
                out.write_bytes(g.label());
                write!(out, "]");
            }
        }
    }
}

#[c_export]
pub static bch_sb_field_ops_disk_groups: c::bch_sb_field_ops =
    sb_field_ops::<c::bch_sb_field_disk_groups>();
