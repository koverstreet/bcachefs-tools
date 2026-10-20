use crate::btree::bkey::{BkeySC, BkeyV, POS_MIN};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeNodeIter};
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::util::kernel::{ktime_get_real_seconds, nsecs_to_jiffies, NSEC_PER_SEC, READ, WRITE};
use crate::util::percpu::acc_u64s_percpu;
use crate::util::workqueue::Queue;
use crate::util::{HumanReadable, Printbuf};
use crate::util::log::{print_str, LogLevel, LogMsg};
use crate::init::error::{count_fsck_err, id};
use crate::init::passes;
use crate::util::rcu::{read_lock, ReadGuard};
use crate::alloc::disk_groups;
use crate::util::ffi::{c_char, Opaque};
use crate::util::strided::{Strided, StridedMut};
use crate::util::vstructs::{bytes_of_mut, plain_bytes, Head, Unsized, Validated};
use core::ffi::{c_int, c_uint, c_ulong, CStr};
use core::sync::atomic::Ordering;
use cstruct_macros::c_export;
use super::io::*;

// ---------------------------------------------------------------------------
// Members — bounds-checked readers for bch_sb_field_members_v2 and v1
//
// Members are variable-size: each entry is `member_bytes` wide, which may
// be smaller than `sizeof(bch_member)` for on-disk backward compatibility.
// The reader copies into a zeroed struct (like the C `bch2_members_v2_get`);
// to edit one, bch_sb_handle::member_mut(), for a member that's all there.
// ---------------------------------------------------------------------------

// Member state names, by value: from BCH_MEMBER_STATES()
macro_rules! member_state_names {
    ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => {
        pub const MEMBER_STATE_NAMES: &[&str] = &[$(stringify!($name),)*];
    };
}
c::BCH_MEMBER_STATES!(member_state_names);

pub fn member_state_str(state: u8) -> &'static str {
    MEMBER_STATE_NAMES.get(state as usize).copied().unwrap_or("unknown")
}

const BCH_MEMBER_V1_BYTES: usize = c::BCH_MEMBER_V1_BYTES as usize;
const MEMBERS_V2_HEAD: usize = size_of::<Head<c::bch_sb_field_members_v2>>();
const MEMBERS_V1_HEAD: usize = size_of::<Head<c::bch_sb_field_members_v1>>();

/// bcachefs_format.h's BCH_SB_MEMBER_DELETED_UUID.
const MEMBER_DELETED_UUID: [u8; 16] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

impl c::bch_member {
    /// Not an empty slot or a deleted member.
    pub fn alive(&self) -> bool {
        self.uuid.b != [0; 16] && !self.deleted()
    }

    /// Removed: its slot isn't free until fsck clears it.
    pub fn deleted(&self) -> bool {
        self.uuid.b == MEMBER_DELETED_UUID
    }
}

pub fn member_exists(sb: &c::bch_sb, dev: u32) -> bool {
    dev < u32::from(sb.nr_devices) && sb_member(sb, dev).is_some_and(|m| m.alive())
}

impl c::bch_sb {
    /// Its members that exist, of nr_devices' slots.
    pub fn number_of_devices(&self) -> u32 {
        (0..u32::from(self.nr_devices)).filter(|&i| member_exists(self, i)).count() as u32
    }
}

/// # Safety
/// @sb is validated.
#[c_export]
pub unsafe fn bch2_sb_nr_devices(sb: *const Head<c::bch_sb>) -> c_uint {
    // SAFETY: the caller's
    unsafe { c::bch_sb::<Validated>::from_ffi(sb) }.number_of_devices()
}

/// A free member slot: past the last one in use if there's room, else the
/// empty one least recently mounted - never BCH_SB_MEMBER_INVALID, the
/// sentinel. Deleted ones aren't free until fsck clears them.
fn sb_member_find_slot(fs: &Fs, sb: &c::bch_sb) -> Result<u32, BchError> {
    let nr = fs.nr_devices();
    if nr < c::BCH_SB_MEMBERS_MAX && nr != c::BCH_SB_MEMBER_INVALID {
        return Ok(nr);
    }

    let mut best: Option<(u64, u32)> = None;
    let mut nr_deleted = 0;

    for dev in (0..c::BCH_SB_MEMBERS_MAX).filter(|&i| i != c::BCH_SB_MEMBER_INVALID) {
        let m = sb_member(sb, dev).unwrap_or_default();
        nr_deleted += m.deleted() as u32;

        let last_mount = m.last_mount.get();
        if m.uuid.b == [0; 16] && best.is_none_or(|(l, _)| last_mount < l) {
            best = Some((last_mount, dev));
        }
    }

    if let Some((_, dev)) = best {
        return Ok(dev);
    }
    if nr_deleted != 0 {
        crate::bch_err!(fs, "unable to allocate new member, but have {nr_deleted} deleted: run fsck");
    }
    Err(bch_errcode::BCH_ERR_ENOSPC_sb_members.into())
}

/// A member slot for a new device, members_v2 and nr_devices grown to hold
/// it.
fn sb_member_alloc(fs: &Fs, sb: &mut SbLockGuard) -> Result<u32, BchError> {
    let dev = sb_member_find_slot(fs, sb.sb())?;
    let nr = (dev + 1).max(fs.nr_devices());

    let stride = usize::from(sb.field::<c::bch_sb_field_members_v2>().expect("members_v2").member_bytes.get());
    let u64s = (MEMBERS_V2_HEAD + stride * nr as usize).div_ceil(size_of::<u64>());

    sb.field_resize::<c::bch_sb_field_members_v2>(u64s as u32)
        .ok_or(BchError::from(bch_errcode::BCH_ERR_ENOSPC_sb_members))?;
    sb.sb_mut().nr_devices = nr as u8;
    Ok(dev)
}

/// The new member's index, or an error.
#[c_export]
pub fn bch2_sb_member_alloc(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    match sb_member_alloc(fs, &mut unsafe { fs.sb_lock_held() }) {
        Ok(dev) => dev as c_int,
        Err(e)  => -e.raw(),
    }
}

/// Deleted members' slots made free: fsck's, once nothing points to them.
#[c_export]
pub fn bch2_sb_members_clean_deleted(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    let mut sb = fs.sb_lock();
    let mut dirty = false;

    for i in 0..fs.nr_devices() {
        let m = sb.fs_member_mut(i);
        if m.deleted() {
            m.uuid.b = [0; 16];
            dirty = true;
        }
    }
    if dirty {
        fs.write_super(&mut sb);
    }
}

/// From members_v2 if the superblock has it, else members_v1.
pub fn sb_member(sb: &c::bch_sb, idx: u32) -> Option<c::bch_member> {
    match members_v2(sb) {
        Some(v2) => v2.get(idx),
        None     => members_v1(sb)?.get(idx),
    }
}

fn members_in(f: &[u8], start: usize, stride: usize) -> Option<Strided<'_, c::bch_member>> {
    Strided::new(f.get(start..)?, stride)
}

/// Read-only view of members_v2 with bounds-checked access.
pub struct MembersV2<'a> {
    members:    Strided<'a, c::bch_member>,
    nr_devices: u32,
}

impl<'a> MembersV2<'a> {
    pub fn get(&self, idx: u32) -> Option<c::bch_member> {
        (idx < self.nr_devices).then(|| self.members.get(idx as usize))?.map(|m| m.get())
    }

    pub fn member_bytes(&self) -> usize {
        self.members.stride()
    }

    pub fn nr_devices(&self) -> u32 {
        self.nr_devices
    }

    pub fn iter(&self) -> impl Iterator<Item = c::bch_member> + '_ {
        (0..self.nr_devices).filter_map(|i| self.get(i))
    }
}

/// Read-only view of members_v1 with bounds-checked access.
pub struct MembersV1<'a> {
    members:    Strided<'a, c::bch_member>,
    nr_devices: u32,
}

impl<'a> MembersV1<'a> {
    pub fn get(&self, idx: u32) -> Option<c::bch_member> {
        (idx < self.nr_devices).then(|| self.members.get(idx as usize))?.map(|m| m.get())
    }

    pub fn nr_devices(&self) -> u32 {
        self.nr_devices
    }

    pub fn iter(&self) -> impl Iterator<Item = c::bch_member> + '_ {
        (0..self.nr_devices).filter_map(|i| self.get(i))
    }
}

/// @ca as C takes a device that may be none: NULL.
pub fn dev_ptr(ca: Option<&c::bch_dev>) -> *mut c::bch_dev {
    ca.map_or(core::ptr::null_mut(), |ca| core::ptr::from_ref(ca).cast_mut())
}

/// Construct a MembersV2 reader from a superblock.
pub fn members_v2(sb: &c::bch_sb) -> Option<MembersV2<'_>> {
    let f = sb_field_get::<c::bch_sb_field_members_v2>(sb)?;
    Some(MembersV2 {
        members: members_in(f.as_bytes(), MEMBERS_V2_HEAD, f.member_bytes.get().into())?,
        nr_devices: sb.nr_devices.into(),
    })
}

/// Construct a MembersV1 reader from a superblock.
pub fn members_v1(sb: &c::bch_sb) -> Option<MembersV1<'_>> {
    let f = sb_field_get::<c::bch_sb_field_members_v1>(sb)?;
    Some(MembersV1 {
        members: members_in(f.as_bytes(), MEMBERS_V1_HEAD, BCH_MEMBER_V1_BYTES)?,
        nr_devices: sb.nr_devices.into(),
    })
}

fn members_v2_mut(mi: &mut c::bch_sb_field_members_v2) -> Option<StridedMut<'_, c::bch_member>> {
    let stride = mi.member_bytes.get().into();
    StridedMut::new(bytes_of_mut(mi).get_mut(MEMBERS_V2_HEAD..)?, stride)
}

impl c::bch_sb_handle {
    /// Member @idx, to edit: None if there's no such member in the section,
    /// or it's shorter than a bch_member - written before it grew.
    pub fn member_mut(&mut self, idx: u32) -> Option<&mut c::bch_member> {
        if idx >= u32::from(self.sb().nr_devices) {
            return None;
        }
        members_v2_mut(self.field_mut::<c::bch_sb_field_members_v2>()?)?.into_whole_mut(idx as usize)
    }

    /// members_v1, the mirror of members_v2 that versions before
    /// extent_flags read - or none, for a superblock they can't.
    pub fn members_cpy_v2_v1(&mut self) -> Result<(), BchError> {
        if self.sb().version_incompat() > c::bcachefs_metadata_version::extent_flags.0.into() {
            self.field_resize::<c::bch_sb_field_members_v1>(0);
            return Ok(());
        }

        let nr = u32::from(self.sb().nr_devices);
        let u64s = (MEMBERS_V1_HEAD + BCH_MEMBER_V1_BYTES * nr as usize).div_ceil(size_of::<u64>());
        self.field_resize::<c::bch_sb_field_members_v1>(u64s as u32)
            .ok_or(BchError::from(bch_errcode::BCH_ERR_ENOSPC_sb_members))?;

        for i in 0..nr {
            let m = members_v2(self.sb()).and_then(|v2| v2.get(i)).expect("members_v2: every member");
            self.put_member_v1::<c::bch_sb_field_members_v1>(MEMBERS_V1_HEAD, i, &m);
        }
        Ok(())
    }

    fn put_member_v1<F: SbField + Unsized + ?Sized>(&mut self, start: usize, i: u32, m: &c::bch_member) {
        let at = start + i as usize * BCH_MEMBER_V1_BYTES;
        let f = bytes_of_mut(self.field_mut::<F>().expect("members: just sized"));
        f[at..at + BCH_MEMBER_V1_BYTES].copy_from_slice(&plain_bytes(m)[..BCH_MEMBER_V1_BYTES]);
    }

    /// After, every member of members_v2 is whole. None if there isn't room.
    pub fn members_v2_init(&mut self) -> Option<()> {
        let nr = u32::from(self.sb().nr_devices);
        let member = size_of::<c::bch_member>();
        let u64s = ((MEMBERS_V2_HEAD + member * nr as usize).div_ceil(size_of::<u64>())) as u32;

        if self.field::<c::bch_sb_field_members_v2>().is_none() {
            let mi = self.field_resize::<c::bch_sb_field_members_v2>(u64s)?;
            mi.pad = Default::default();
            mi.member_bytes = (BCH_MEMBER_V1_BYTES as u16).into();

            for i in 0..nr {
                let m = members_v1(self.sb()).and_then(|v1| v1.get(i)).unwrap_or_default();
                self.put_member_v1::<c::bch_sb_field_members_v2>(MEMBERS_V2_HEAD, i, &m);
            }
        }

        let stride: usize = self.field::<c::bch_sb_field_members_v2>()?.member_bytes.get().into();
        if stride < member {
            let mi = self.field_resize::<c::bch_sb_field_members_v2>(u64s)?;
            StridedMut::<c::bch_member>::grow(&mut bytes_of_mut(mi)[MEMBERS_V2_HEAD..], nr as usize, stride, member)
                .expect("members_v2: validated, and just sized");
            mi.member_bytes = (member as u16).into();
        }
        Some(())
    }
}

#[c_export]
pub fn bch2_sb_members_v2_init(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    let mut sb = unsafe { fs.sb_lock_held() };
    ret_to_c(sb.members_v2_init().ok_or_else(|| fs.err(bch_errcode::BCH_ERR_ENOSPC_sb_members_v2)))
}

/// # Safety
/// @disk_sb is valid, and nothing else uses it meanwhile.
#[c_export]
pub unsafe fn bch2_sb_members_cpy_v2_v1(disk_sb: *mut c::bch_sb_handle) -> c_int {
    // SAFETY: the caller's
    ret_to_c(unsafe { &mut *disk_sb }.members_cpy_v2_v1())
}

/// Member @i of members_v2, whole - as every one is, on an open filesystem.
///
/// # Safety
/// @sb is validated, and nothing else uses it meanwhile: under sb_lock.
#[c_export]
pub unsafe fn bch2_members_v2_get_mut(sb: *mut Head<c::bch_sb>, i: c_int) -> *mut c::bch_member {
    // SAFETY: the caller's
    unsafe { sb_field_get_raw_mut::<c::bch_sb_field_members_v2>(sb) }
        .and_then(members_v2_mut)
        .and_then(|m| m.into_whole_mut(i as usize))
        .expect("members_v2: a member, whole")
}

/// Member @i, from members_v2 or members_v1: zero if there's none.
///
/// # Safety
/// @sb is validated.
#[c_export]
pub unsafe fn bch2_sb_member_get(sb: *mut Head<c::bch_sb>, i: c_int) -> c::bch_member {
    // SAFETY: the caller's
    let sb = unsafe { c::bch_sb::<Validated>::from_ffi(sb) };
    sb_member(sb, i as u32).unwrap_or_default()
}

// ── Validating, and printing ─────────────────────────────────────────────

macro_rules! member_error_names {
    ($(($name:tt, $n:tt)),* $(,)?) => { const MEMBER_ERROR_NAMES: &[&str] = &[$(stringify!($name),)*]; };
}
c::BCH_MEMBER_ERROR_TYPES!(member_error_names);

macro_rules! iops_names {
    ($(($name:tt, $n:tt)),* $(,)?) => { const IOPS_NAMES: &[&str] = &[$(stringify!($name),)*]; };
}
c::BCH_IOPS_MEASUREMENTS!(iops_names);

/// A __nonstring field: NUL terminated only if shorter than the field.
fn nonstring(b: &[u8]) -> &[u8] {
    &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]
}

fn invalid() -> Result<(), BchError> {
    Err(bch_errcode::BCH_ERR_invalid_sb_members.into())
}

fn validate_member(err: &mut Printbuf, m: &c::bch_member, sb: &c::bch_sb, i: u32) -> Result<(), BchError> {
    let nbuckets = m.nbuckets.get();
    if nbuckets > c::BCH_MEMBER_NBUCKETS_MAX.into() {
        write!(err, "device {i}: too many buckets (got {nbuckets}, max {})", c::BCH_MEMBER_NBUCKETS_MAX);
        return invalid();
    }

    // wrapping, as C's: a first_bucket past nbuckets isn't caught here
    let usable = nbuckets.wrapping_sub(m.first_bucket.get().into());
    if usable < c::BCH_MIN_NR_NBUCKETS.into() {
        write!(err, "device {i}: not enough buckets (got {usable}, min {})", c::BCH_MIN_NR_NBUCKETS);
        return invalid();
    }

    let (bucket_size, block_size) = (m.bucket_size.get(), sb.block_size.get());
    if bucket_size < block_size {
        write!(err, "device {i}: bucket size {bucket_size} smaller than block size {block_size}");
        return invalid();
    }

    if u64::from(bucket_size) < sb.btree_node_size() {
        write!(err, "device {i}: bucket size {bucket_size} smaller than btree node size {}", sb.btree_node_size());
        return invalid();
    }

    if u32::from(m.btree_bitmap_shift) >= c::BCH_MI_BTREE_BITMAP_SHIFT_MAX {
        write!(err, "device {i}: invalid btree_bitmap_shift {}", m.btree_bitmap_shift);
        return invalid();
    }

    if m.freespace_initialized() &&
       sb.features[0].get() & (1 << c::bch_sb_feature::BCH_FEATURE_no_alloc_info.0) != 0 {
        write!(err, "device {i}: freespace initialized but fs has no alloc info");
        return invalid();
    }
    Ok(())
}

fn members_validate(sb: &c::bch_sb, members: Option<Strided<'_, c::bch_member>>,
                    err: &mut Printbuf) -> Result<(), BchError> {
    let nr = usize::from(sb.nr_devices);
    let have = members.as_ref().map_or(0, |m| m.len());
    let Some(members) = members.filter(|_| have >= nr) else {
        write!(err, "section too small: {nr} devices, room for {have}");
        return invalid();
    };

    for i in 0..nr {
        validate_member(err, &members.get(i).unwrap().get(), sb, i as u32)?;
    }
    Ok(())
}

/// Every member there's room for, nr_devices or not: printed sections
/// include ones that failed validation.
fn members_to_text(out: &mut Printbuf, sb: &c::bch_sb, members: Option<Strided<'_, c::bch_member>>) {
    let Some(members) = members else {
        write!(out, "member_bytes 0");
        return;
    };

    let nr = usize::from(sb.nr_devices);
    if members.len() != nr {
        write!(out, "nr_devices mismatch: have {} entries, should be {nr}", members.len());
    }

    for i in 0..nr.min(members.len()) {
        let m = members.get(i).unwrap().get();
        if m.alive() {
            write!(out, "Device:\t{i}\n");
            member_to_text(&mut out.indent(2), &m, sb, i as u32);
        }
    }
}

impl SbFieldOps for c::bch_sb_field_members_v1 {
    fn validate(sb: &c::bch_sb, f: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
        members_validate(sb, members_in(f.as_bytes(), MEMBERS_V1_HEAD, BCH_MEMBER_V1_BYTES), err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, sb: &c::bch_sb, f: &Self) {
        members_to_text(out, sb, members_in(f.as_bytes(), MEMBERS_V1_HEAD, BCH_MEMBER_V1_BYTES));
    }
}

impl SbFieldOps for c::bch_sb_field_members_v2 {
    fn validate(sb: &c::bch_sb, f: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
        members_validate(sb, members_in(f.as_bytes(), MEMBERS_V2_HEAD, f.member_bytes.get().into()), err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, sb: &c::bch_sb, f: &Self) {
        members_to_text(out, sb, members_in(f.as_bytes(), MEMBERS_V2_HEAD, f.member_bytes.get().into()));
    }
}

#[c_export]
pub static bch_sb_field_ops_members_v1: c::bch_sb_field_ops = sb_field_ops::<c::bch_sb_field_members_v1>();

#[c_export]
pub static bch_sb_field_ops_members_v2: c::bch_sb_field_ops = sb_field_ops::<c::bch_sb_field_members_v2>();

fn data_types_to_text(out: &mut Printbuf, types: u64) {
    match types {
        // SAFETY: a NULL terminated table of names
        t if t != 0 => unsafe { out.prt_bitflags(c::__bch2_data_types.as_ptr(), t) },
        _           => write!(out, "(none)"),
    }
}

fn label_to_text(out: &mut Printbuf, m: &c::bch_member, sb: &c::bch_sb) {
    write!(out, "Label:\t");
    match m.group() {
        0 => write!(out, "(none)"),
        g => disk_groups::disk_path_to_text_sb(out, sb, g as u32 - 1),
    }
    out.newline();
}

fn failure_domain_to_text(out: &mut Printbuf, m: &c::bch_member) {
    if m.failure_domain[0] != 0 {
        write!(out, "Failure domain:\t");
        out.write_bytes(nonstring(&m.failure_domain));
        out.newline();
    }
}

fn has_data_to_text(out: &mut Printbuf, sb: &c::bch_sb, idx: u32) {
    write!(out, "Has data:\t");
    data_types_to_text(out, sb.dev_has_data(idx).into());
    out.newline();
}

/// show-super's member: every field.
pub fn member_to_text(out: &mut Printbuf, m: &c::bch_member, sb: &c::bch_sb, idx: u32) {
    let bucket_size = u64::from(m.bucket_size.get());

    label_to_text(out, m, sb);
    failure_domain_to_text(out, m);
    write!(out, "UUID:\t{}\n", uuid::Uuid::from_bytes(m.uuid.b));
    write!(out, "Size:\t");
    out.units_u64((m.nbuckets.get() * bucket_size) << 9);
    out.newline();

    for (name, e) in MEMBER_ERROR_NAMES.iter().zip(&m.errors) {
        write!(out, "{name} errors:\t{}\n", e.get());
    }
    for (name, iops) in IOPS_NAMES.iter().zip(&m.iops) {
        write!(out, "{name} iops:\t{}\n", iops.get());
    }

    write!(out, "Bucket size:\t");
    out.units_u64(bucket_size << 9);
    out.newline();
    write!(out, "First bucket:\t{}\n", m.first_bucket.get());
    write!(out, "Buckets:\t{}\n", m.nbuckets.get());

    write!(out, "Last mount:\t");
    match m.last_mount.get() {
        0 => write!(out, "(never)"),
        t => out.datetime(t as i64),
    }
    out.newline();
    write!(out, "Last superblock write:\t{}\n", m.seq.get());

    write!(out, "State:\t{}\n", member_state_str(m.state() as u8));
    write!(out, "Data allowed:\t");
    data_types_to_text(out, m.data_allowed());
    out.newline();
    has_data_to_text(out, sb, idx);
    write!(out, "Rotational:\t{}\n", m.rotational() as u8);

    write!(out, "Btree allocated bitmap blocksize:\t");
    match m.btree_bitmap_shift {
        s if s < 64 => out.units_u64(1 << s),
        s           => write!(out, "(invalid shift {s})"),
    }
    out.newline();
    write!(out, "Btree allocated bitmap:\t");
    // SAFETY: only formats
    unsafe { c::bch2_prt_u64_base2_nbits(out.as_raw(), m.btree_allocated_bitmap.get(), 64) };
    out.newline();

    write!(out, "Durability:\t{}\n", m.durability().checked_sub(1).unwrap_or(1));
    write!(out, "Discard:\t{}\n", m.discard() as u8);
    write!(out, "Freespace initialized:\t{}\n", m.freespace_initialized() as u8);
    write!(out, "Resize on mount:\t{}\n", m.resize_on_mount() as u8);

    for (name, f) in [("Last device name", &m.device_name[..]),
                      ("Last device model", &m.device_model[..]),
                      ("Last device serial", &m.device_serial[..])] {
        write!(out, "{name}:\t");
        out.write_bytes(nonstring(f));
        out.newline();
    }

    if m.flush_errors.get() != 0 {
        write!(out, "Flush errors:\t{}\n", m.flush_errors.get());
    }
}

/// For messages, to say which disk is meant: what's empty is left out. The
/// superblock's alone, for mount to name a device it hasn't opened.
fn member_to_text_short(out: &mut Printbuf, m: &c::bch_member, sb: &c::bch_sb, idx: u32) {
    if out.nr_tabstops() == 0 {
        out.tabstop_push(16 + out.indent_level());
    }

    if m.group() != 0 {
        label_to_text(out, m, sb);
    }
    failure_domain_to_text(out, m);

    for (name, f) in [("Device", &m.device_name[..]),
                      ("Model", &m.device_model[..]),
                      ("Serial", &m.device_serial[..])] {
        if f[0] != 0 {
            write!(out, "{name}:\t");
            out.write_bytes(nonstring(f));
            out.newline();
        }
    }

    write!(out, "State:\t{}\n", member_state_str(m.state() as u8));
    has_data_to_text(out, sb, idx);
}

fn dev_to_text_short(out: &mut Printbuf, sb: &c::bch_sb, dev: u32) {
    member_to_text_short(out, &sb_member(sb, dev).unwrap_or_default(), sb, dev);
}

#[c_export]
pub fn bch2_member_to_text_short_locked(out: &mut Printbuf, c: &Opaque<c::bch_fs>, ca: &c::bch_dev) {
    // SAFETY: C calls it under sb_lock
    dev_to_text_short(out, unsafe { Fs::from_c(c).sb_lock_held() }.sb(), ca.dev_idx.into());
}

#[c_export]
pub fn bch2_member_to_text_short(out: &mut Printbuf, c: &Opaque<c::bch_fs>, ca: &c::bch_dev) {
    let fs = Fs::from_c(c);
    dev_to_text_short(out, fs.sb_lock().sb(), ca.dev_idx.into());
}

#[c_export]
pub fn bch2_devs_mask_to_text_locked(out: &mut Printbuf, c: &Opaque<c::bch_fs>, devs: &c::bch_devs_mask) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    let sb = unsafe { fs.sb_lock_held() };
    for dev in devs.d.iter() {
        if fs.dev_rcu(dev as u32, &read_lock()).is_some() {
            dev_to_text_short(out, sb.sb(), dev as u32);
        }
    }
}

// ── In memory: ca->mi, and what goes back ────────────────────────────────
//
// ca->mi is the superblock's member, decoded: written under sb_lock - set_mi()
// takes the guard - and read without it, as C's readers always have, so a
// reader can see a copy half made.

impl c::bch_member {
    /// As ca->mi has it - all but the failure domain, which takes the other
    /// members to number: bch2_mi_to_cpu().
    pub fn to_cpu(&self) -> c::bch_member_cpu {
        let nbuckets = self.nbuckets.get();
        let first_bucket = self.first_bucket.get();

        c::bch_member_cpu {
            nbuckets,
            nbuckets_minus_first:   nbuckets.wrapping_sub(first_bucket.into()),
            first_bucket,
            bucket_size:            self.bucket_size.get(),
            group:                  self.group() as u16,
            failure_domain:         0,
            state:                  self.state() as u8,
            discard:                self.discard() as u8,
            data_allowed:           self.data_allowed() as u8,
            // one-biased on disk: 0 is the default, 1
            durability:             self.durability().checked_sub(1).unwrap_or(1) as u8,
            freespace_initialized:  self.freespace_initialized() as u8,
            initialized:            self.initialized() as u8,
            resize_on_mount:        self.resize_on_mount() as u8,
            rotational:             self.rotational() as u8,
            valid:                  self.alive() as u8,
            btree_bitmap_shift:     self.btree_bitmap_shift,
            btree_allocated_bitmap: self.btree_allocated_bitmap.get(),
        }
    }
}

#[c_export]
pub fn bch2_mi_to_cpu(mi: &c::bch_member) -> c::bch_member_cpu {
    mi.to_cpu()
}

/// Member @dev's failure domain, as an id: one more than the first member
/// with the same string, 0 for none. Only compared for equality, so it
/// needn't be dense.
fn failure_domain_id(sb: &c::bch_sb, dev: u32, m: &c::bch_member) -> u16 {
    if m.failure_domain[0] == 0 {
        return 0;
    }
    (0..=dev).find(|&i| sb_member(sb, i).unwrap_or_default().failure_domain == m.failure_domain)
        .map_or(0, |i| i as u16 + 1)
}

/// ca->mi, and the masks of rotational and removed devices, from the
/// superblock.
fn members_to_cpu(fs: &Fs, sb: &SbLockGuard) {
    for ca in fs.members() {
        let dev = u32::from(ca.dev_idx);
        let m = sb_member(sb.sb(), dev).unwrap_or_default();
        let mut mi = m.to_cpu();
        mi.failure_domain = failure_domain_id(sb.sb(), dev, &m);

        ca.set_mi(sb, mi);
        fs.devs_rotational.assign(dev as usize, mi.rotational != 0);
    }

    if let Some(mi2) = members_v2(sb.sb()) {
        for i in 0..fs.nr_devices() {
            fs.devs_removed.assign(i as usize, mi2.get(i).unwrap_or_default().deleted());
        }
    }
}

#[c_export]
pub fn bch2_sb_members_to_cpu(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    members_to_cpu(fs, &unsafe { fs.sb_lock_held() });
}

/// The error counts, into the superblock's members - for a superblock write.
#[c_export]
pub fn bch2_sb_members_from_cpu(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    let mut sb = unsafe { fs.sb_lock_held() };

    let rcu = read_lock();
    for ca in fs.devs_rcu(&rcu) {
        let m = sb.fs_member_mut(ca.dev_idx.into());
        for (d, e) in m.errors.iter_mut().zip(&ca.errors) {
            *d = (e.load(Ordering::Relaxed) as u64).into();
        }
    }
}

/// The worst IO latency seen, reading or writing as @rw, of @devs, in jiffies:
/// to scale timeouts and "held too long" warnings so slow storage doesn't
/// trip them.
#[c_export]
pub fn bch2_dev_latency_max(c: &Opaque<c::bch_fs>, devs: &c::bch_devs_mask, rw: c_int) -> c_ulong {
    let fs = Fs::from_c(c);
    let rcu = read_lock();
    let ns = fs.devs_rcu_in(devs.d.iter(), &rcu)
        .map(|ca| ca.io_latency[rw as usize].stats.max_duration)
        .max()
        .unwrap_or(0);
    nsecs_to_jiffies(ns)
}

// ── Device identity, recorded in the member ──────────────────────────────

/// @dst, one of the member's fixed-size strings, made @src - NUL padded, cut
/// to fit - unless @src is empty. Whether it changed.
fn mi_update_str(dst: &mut [u8], src: &[c_char]) -> bool {
    let src = &src[..src.len().min(dst.len())];
    let n = src.iter().position(|&b| b == 0).unwrap_or(src.len());
    if n == 0 {
        return false;
    }

    let mut changed = false;
    for (i, d) in dst.iter_mut().enumerate() {
        let b = if i < n { src[i] as u8 } else { 0 };
        changed |= *d != b;
        *d = b;
    }
    changed
}

/// Member @dev's name, model and serial made @id's, and its rotational
/// flag if it's never been set. Whether anything changed.
fn mi_field_upgrades(sb: &mut SbLockGuard, dev: u32, id: &c::bch_dev_identity) -> bool {
    let m = sb.fs_member_mut(dev);
    let mut dirty = false;

    dirty |= mi_update_str(&mut m.device_name, &id.name);
    dirty |= mi_update_str(&mut m.device_model, &id.model);
    dirty |= mi_update_str(&mut m.device_serial, &id.serial);

    if !m.rotational_set() {
        m.set_rotational(id.rotational);
        m.set_rotational_set(true);
        dirty = true;
    }
    dirty
}

/// @w, the caller's superblock write, is dirtied if anything changed.
#[c_export]
pub fn bch2_dev_mi_field_upgrades_locked(c: &Opaque<c::bch_fs>, ca: &c::bch_dev,
                                         id: &c::bch_dev_identity, w: &mut c::sb_write) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    w.dirty |= mi_field_upgrades(&mut unsafe { fs.sb_lock_held() }, ca.dev_idx.into(), id);
}

#[c_export]
pub fn bch2_dev_mi_field_read(ca: &c::bch_dev, id: &mut c::bch_dev_identity) {
    *id = ca.identity();
}

#[c_export]
pub fn bch2_dev_mi_field_upgrades(ca: &c::bch_dev) {
    ca.mi_field_upgrades();
}

/// Every online device's identity, recorded.
#[c_export]
pub fn bch2_fs_mi_field_upgrades(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    for ca in fs.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_fs_mi_field_upgrades) {
        /*
         * Reads sysfs, so it can't happen under sb_lock - which is why this
         * writes per dirty device instead of once at the end. Only the first
         * mount after these fields were added dirties more than one.
         */
        ca.mi_field_upgrades();
    }
}

// ── Where btree nodes are: each member's bitmap ──────────────────────────
//
// So that if we ever have to scan for btree nodes to repair, we needn't scan
// whole devices: a bit per 1 << btree_bitmap_shift sectors, 64 of them, set
// for every range a btree node has been written to. The shift grows - the
// bitmap coarsens - as nodes are written further out. gc rebuilds it into
// ca->btree_allocated_bitmap_gc, and replaces it.

/// The bits of a bitmap at @shift that cover sectors [@start, @end).
fn bitmap_bits(start: u64, end: u64, shift: u8) -> u64 {
    (start >> shift..).take_while(|&bit| bit << shift < end).fold(0, |b, bit| b | 1 << bit)
}

/// @bitmap, its shift grown by @by: each bit folded into the one now covering it.
fn bitmap_coarsen(bitmap: u64, by: u32) -> u64 {
    (0..64).filter(|i| bitmap & 1 << i != 0).fold(0, |new, i| new | 1 << (i >> by))
}

impl c::bch_dev {
    /// Whether all of [@start, @start + @sectors) is marked in its member's
    /// bitmap - and, @with_gc, in the one gc is rebuilding. members.h has
    /// C's, an inline the allocator calls.
    fn btree_bitmap_marked_sectors(&self, start: u64, sectors: u32, with_gc: bool) -> bool {
        let shift = self.mi.btree_bitmap_shift;
        let end = start + u64::from(sectors);
        if end > 64u64 << shift {
            return false;
        }

        let gc = if with_gc { self.btree_allocated_bitmap_gc } else { !0 };
        let want = bitmap_bits(start, end, shift);
        self.mi.btree_allocated_bitmap & gc & want == want
    }

    /// ca->btree_allocated_bitmap_gc made @v: written under sb_lock, @_sb,
    /// and read without it, as ca->mi.
    fn set_btree_bitmap_gc(&self, _sb: &SbLockGuard, v: u64) {
        // SAFETY: as set_mi()
        unsafe { (&raw const self.btree_allocated_bitmap_gc).cast_mut().write(v) }
    }

    /// [@start, @start + @sectors) marked in its member's bitmap, and in gc's
    /// - the bitmap coarsened first if it doesn't reach that far. Whether the
    /// member changed.
    fn btree_bitmap_mark_sectors(&self, sb: &mut SbLockGuard, start: u64, sectors: u32) -> bool {
        let end = start + u64::from(sectors);
        let mut gc = self.btree_allocated_bitmap_gc;
        let mut dirty = false;

        let m = sb.fs_member_mut(self.dev_idx.into());

        let resize = end.next_power_of_two().trailing_zeros() as i32 - (i32::from(m.btree_bitmap_shift) + 6);
        if resize > 0 {
            m.btree_allocated_bitmap = bitmap_coarsen(m.btree_allocated_bitmap.get(), resize as u32).into();
            m.btree_bitmap_shift += resize as u8;
            gc = bitmap_coarsen(gc, resize as u32);
            dirty = true;
        }

        let shift = m.btree_bitmap_shift;
        assert!(u32::from(shift) < c::BCH_MI_BTREE_BITMAP_SHIFT_MAX);
        assert!(end <= 64u64 << shift);

        let bits = bitmap_bits(start, end, shift);
        let old = m.btree_allocated_bitmap.get();
        dirty |= old | bits != old;
        m.btree_allocated_bitmap = (old | bits).into();

        self.set_btree_bitmap_gc(sb, gc | bits);
        dirty
    }
}

impl Fs {
    /// Whether every device @k points to has its node marked - a device
    /// that isn't there doesn't count against it.
    fn btree_bitmap_marked(&self, k: BkeySC<'_>, with_gc: bool) -> bool {
        let Some(k) = k.arm::<Ptrs>() else { return true };
        let rcu = read_lock();
        let marked = k.ptrs(self).all(|p| self.dev_rcu(p.dev().into(), &rcu)
            .is_none_or(|ca| ca.btree_bitmap_marked_sectors(p.offset(), self.btree_sectors(), with_gc)));
        marked
    }

    /// @k's node marked on each device it points to. Whether a member
    /// changed.
    fn btree_bitmap_mark_locked(&self, sb: &mut SbLockGuard, k: BkeySC<'_>) -> bool {
        let Some(k) = k.arm::<Ptrs>() else { return false };
        let rcu = read_lock();
        let mut dirty = false;
        for p in k.ptrs(self) {
            if let Some(ca) = self.dev_rcu(p.dev().into(), &rcu) {
                dirty |= ca.btree_bitmap_mark_sectors(sb, p.offset(), self.btree_sectors());
            }
        }
        dirty
    }

    fn btree_bitmap_mark(&self, k: BkeySC<'_>) {
        let mut sb = self.sb_lock();
        if self.btree_bitmap_mark_locked(&mut sb, k) {
            self.write_super(&mut sb);
        }
    }

    fn btree_bitmap_mark_if_unmarked(&self, k: BkeySC<'_>) {
        if !self.btree_bitmap_marked(k, true) {
            self.btree_bitmap_mark(k);
        }
    }

    /// The bitmaps rebuilt from the btree nodes there are: every interior
    /// node's key, and each root's.
    fn btree_bitmap_gc(&self) -> Result<(), BchError> {
        let progress = &self.recovery.progress;
        progress.init_mask(self, c"bch2_btree_bitmap_gc", 0, !0);

        {
            let sb = self.sb_lock();
            let rcu = read_lock();
            for ca in self.devs_rcu(&rcu) {
                ca.set_btree_bitmap_gc(&sb, 0);
            }
        }

        {
            let trans = crate::btree_trans!(self);

            for btree in 0..self.btree_id_nr_alive() {
                for level in 1..c::BTREE_MAX_DEPTH {
                    BtreeIter::new_level(&trans, btree, POS_MIN, level, BtreeIterFlags::PREFETCH)
                        .for_each(&trans, |t, iter, k| {
                            self.btree_bitmap_mark_if_unmarked(k);
                            progress.update(t, iter)
                        })?;
                }

                let root = self.btree_id_root(btree).expect("a root for every btree");
                let mut iter = BtreeNodeIter::new(&trans, btree, POS_MIN, 0, root.c.level.into(),
                                                  BtreeIterFlags::empty());
                lockrestart_do(&trans, |_| {
                    let b = iter.peek()?.expect("the root node");
                    self.btree_bitmap_mark_if_unmarked(b.key_i().as_sc());
                    Ok(())
                })?;
            }
        }

        let (mut old, mut new) = (0u64, 0u64);
        {
            let mut sb = self.sb_lock();
            {
                let rcu = read_lock();
                for ca in self.devs_rcu(&rcu) {
                    let shift = ca.mi.btree_bitmap_shift;
                    old += u64::from(ca.mi.btree_allocated_bitmap.count_ones()) << shift;
                    new += u64::from(ca.btree_allocated_bitmap_gc.count_ones()) << shift;

                    sb.fs_member_mut(ca.dev_idx.into()).btree_allocated_bitmap = ca.btree_allocated_bitmap_gc.into();
                }
            }
            self.write_super(&mut sb);
        }

        crate::bch_info!(self, "mi_btree_bitmap sectors {} -> {}",
                         HumanReadable(old << 9), HumanReadable(new << 9));
        Ok(())
    }
}

#[c_export]
pub fn bch2_dev_btree_bitmap_marked(c: &Opaque<c::bch_fs>, k: BkeySC<'_>) -> bool {
    Fs::from_c(c).btree_bitmap_marked(k, true)
}

#[c_export]
pub fn bch2_dev_btree_bitmap_marked_nogc(c: &Opaque<c::bch_fs>, k: BkeySC<'_>) -> bool {
    Fs::from_c(c).btree_bitmap_marked(k, false)
}

/// @w, the caller's superblock write, is dirtied if a member changed.
#[c_export]
pub fn bch2_dev_btree_bitmap_mark_locked(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, w: &mut c::sb_write) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    w.dirty |= fs.btree_bitmap_mark_locked(&mut unsafe { fs.sb_lock_held() }, k);
}

#[c_export]
pub fn bch2_dev_btree_bitmap_mark(c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    Fs::from_c(c).btree_bitmap_mark(k)
}

#[c_export]
pub fn bch2_btree_bitmap_gc(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(Fs::from_c(c).btree_bitmap_gc())
}

crate::fs_delayed_work! {
    /// Daily, while rw: btree_bitmap_gc scheduled - ratelimited - if a
    /// device's bitmap marks four times the sectors of its btree buckets.
    pub(crate) work BtreeBitmapGcSchedule: field maybe_schedule_btree_bitmap_gc;

    fn run(fs: &Fs) {
        if passes::want_ratelimit(fs, c::bch_recovery_pass::BCH_RECOVERY_PASS_btree_bitmap_gc, 1000) {
            return;
        }

        let mut msg = LogMsg::with_level(fs, LogLevel::NOTICE);
        msg.set_suppressed(true); // run_explicit() unsuppresses

        let mut want_schedule = false;
        for ca in fs.members() {
            let u = ca.usage_read_fast();
            let btree_sectors = ca.bucket_to_sector(u.buckets[c::bch_data_type::BCH_DATA_btree.0 as usize]);
            let bitmap_sectors = u64::from(ca.mi.btree_allocated_bitmap.count_ones()) << ca.mi.btree_bitmap_shift;

            if btree_sectors * 4 < bitmap_sectors {
                write!(msg, "{} has ", ca.name().to_str().unwrap_or("?"));
                msg.human_readable_u64(btree_sectors << 9);
                write!(msg, " btree buckets and ");
                msg.human_readable_u64(bitmap_sectors << 9);
                write!(msg, " marked in bitmap\n");
                want_schedule = true;
            }
        }

        if want_schedule {
            // Ignored, as C ignored it
            let _ = passes::run_explicit(fs, &mut msg, c::bch_recovery_pass::BCH_RECOVERY_PASS_btree_bitmap_gc,
                                         c::bch_run_recovery_pass_flags::RUN_RECOVERY_PASS_ratelimit);
        }

        fs.maybe_schedule_btree_bitmap_gc.queue(Queue::system_long(), nsecs_to_jiffies(24 * 60 * 60 * NSEC_PER_SEC));
    }
}

/// Going rw: the check now, and daily after.
#[c_export]
pub fn bch2_maybe_schedule_btree_bitmap_gc(c: &Opaque<c::bch_fs>) {
    BtreeBitmapGcSchedule::run(Fs::from_c(c));
}

#[c_export]
pub fn bch2_maybe_schedule_btree_bitmap_gc_stop(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).maybe_schedule_btree_bitmap_gc.cancel_sync();
}

// ---------------------------------------------------------------------------
// Devices - c->devs[], the filesystem's bch_devs, and what's known of each:
// members.h's in-memory side.
//
// c->devs[] is RCU: a removed device's slot is cleared, and its bch_dev freed
// only after a grace period. So a lookup without a ref - dev_rcu() - is good
// for as long as the reader's ReadGuard; one that has to outlive it takes a
// ref, Fs::dev_get().
// ---------------------------------------------------------------------------

/// A short list of devices - an extent's, a write point's: at most
/// BCH_BKEY_PTRS_MAX, no repeats.
impl c::bch_devs_list {
    /// Just @dev: bch2_dev_list_single().
    pub fn single(dev: u8) -> Self {
        let mut l = Self::default();
        l.add(dev);
        l
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.nr as usize]
    }

    /// bch2_dev_list_has_dev().
    pub fn has(&self, dev: u8) -> bool {
        self.as_slice().contains(&dev)
    }

    /// Add @dev, if it isn't in it already: bch2_dev_list_add_dev(). A list
    /// already full is a bug, as C's BUG_ON().
    pub fn add(&mut self, dev: u8) {
        if !self.has(dev) {
            assert!((self.nr as usize) < self.data.len(), "bch_devs_list full adding device {dev}");
            self.data[self.nr as usize] = dev;
            self.nr += 1;
        }
    }

    /// Remove @dev, keeping the rest in order: bch2_dev_list_drop_dev().
    pub fn drop(&mut self, dev: u8) {
        if let Some(i) = self.as_slice().iter().position(|&d| d == dev) {
            self.data.copy_within(i + 1..self.nr as usize, i);
            self.nr -= 1;
        }
    }
}

impl Fs {
    /// c->devs[@dev], for as long as @rcu's section: bch2_dev_rcu_noerror().
    /// None past the superblock's devices, or for a slot with no device.
    pub fn dev_rcu<'g>(&'g self, dev: u32, rcu: &'g ReadGuard) -> Option<&'g c::bch_dev> {
        self.devs.get(..self.nr_devices() as usize)?.get(dev as usize)?.read(rcu)
    }

    /// The filesystem's devices, for as long as @rcu's section:
    /// for_each_member_device_rcu().
    pub fn devs_rcu<'g>(&'g self, rcu: &'g ReadGuard) -> impl Iterator<Item = &'g c::bch_dev> {
        // nr_devices read each step, as C's: a device added meanwhile is seen
        (0..).take_while(move |&dev| dev < self.nr_devices())
            .filter_map(move |dev| self.dev_rcu(dev, rcu))
    }

    /// Those of @devs - a mask's iter() - that there are:
    /// for_each_member_device_rcu() with a mask.
    pub fn devs_rcu_in<'g>(&'g self, devs: impl Iterator<Item = usize> + 'g, rcu: &'g ReadGuard)
        -> impl Iterator<Item = &'g c::bch_dev>
    {
        devs.filter_map(move |dev| self.dev_rcu(dev as u32, rcu))
    }

    /// The online devices: for_each_online_member_rcu().
    pub fn online_devs_rcu<'g>(&'g self, rcu: &'g ReadGuard) -> impl Iterator<Item = &'g c::bch_dev> {
        self.devs_rcu_in(self.devs_online.iter(), rcu)
    }

    /// The devices the allocator writes to: for_each_rw_member_rcu().
    pub fn rw_devs_rcu<'g>(&'g self, rcu: &'g ReadGuard) -> impl Iterator<Item = &'g c::bch_dev> {
        self.devs_rcu_in(self.allocator.rw_devs[c::bch_data_type::BCH_DATA_free.0 as usize].iter(), rcu)
    }

    /// dev_rcu(), a missing device reported: bch2_dev_rcu().
    pub fn dev_rcu_or_err<'g>(&'g self, dev: u32, rcu: &'g ReadGuard) -> Result<&'g c::bch_dev, BchError> {
        self.dev_rcu(dev, rcu).ok_or_else(|| self.dev_missing(dev))
    }

    /// dev_missing_atomic(), and the error for it.
    fn dev_missing(&self, dev: u32) -> BchError {
        self.dev_missing_atomic(dev);
        self.err(bch_errcode::BCH_ERR_ENOENT_dev_not_found)
    }

    /// Whether there's a device @dev: bch2_dev_exists().
    pub fn dev_exists(&self, dev: u32) -> bool {
        self.dev_rcu(dev, &read_lock()).is_some()
    }

    /// A ref on device @dev, if there is one: bch2_dev_tryget_noerror().
    pub fn dev_get(&self, dev: u32) -> Option<DevRef> {
        self.dev_rcu(dev, &read_lock()).map(c::bch_dev::get_ref)
    }

    /// The first device at index @dev or after, with a ref: __bch2_next_dev_idx().
    pub fn next_dev(&self, dev: u32) -> Option<DevRef> {
        (dev..self.nr_devices()).find_map(|i| self.dev_get(i))
    }

    /// dev_get(), a missing device reported: bch2_dev_tryget().
    pub fn dev_tryget(&self, dev: u32) -> Result<DevRef, BchError> {
        self.dev_get(dev).ok_or_else(|| self.dev_missing(dev))
    }

    /// dev_get() for a pointer of @k's - bch2_dev_bkey_tryget(). A missing
    /// device isn't an error: it's reported against @k and check_allocations
    /// scheduled to repair it (unless it's BCH_SB_MEMBER_INVALID, which names
    /// none), and we carry on without it. The error is scheduling's, e.g.
    /// restart_recovery.
    pub fn dev_bkey_tryget(&self, k: BkeySC<'_>, dev: u32) -> Result<Option<DevRef>, BchError> {
        let ca = self.dev_get(dev);
        if ca.is_none() && dev != c::BCH_SB_MEMBER_INVALID {
            self.dev_missing_bkey(k, dev)?;
        }
        Ok(ca)
    }

    /// A ref on @bucket's device, if it's a device with that bucket:
    /// bch2_dev_bucket_tryget_noerror().
    pub fn dev_bucket_tryget_noerror(&self, bucket: c::bpos) -> Option<DevRef> {
        self.dev_get(bucket.inode as u32).filter(|ca| ca.bucket_valid(bucket.offset))
    }

    /// dev_bucket_tryget_noerror(), a missing device or bucket reported:
    /// bch2_dev_bucket_tryget().
    pub fn dev_bucket_tryget(&self, bucket: c::bpos) -> Result<DevRef, BchError> {
        let ca = self.dev_tryget(bucket.inode as u32)?;
        if !ca.bucket_valid(bucket.offset) {
            ca.bucket_missing(bucket.offset);
            return Err(self.err(bch_errcode::BCH_ERR_ENOENT_dev_bucket_not_found));
        }
        Ok(ca)
    }

    /// An io ref on device @dev, as @ref_idx - for writing only if it's rw:
    /// bch2_dev_get_ioref().
    pub fn dev_get_ioref<R: IoRefIdx>(&self, dev: u32, ref_idx: R) -> Option<DevIoRef<'_, R>> {
        let rcu = read_lock();
        let ca = DevIoRef::tryget(self.dev_rcu(dev, &rcu)?, ref_idx)?;
        // then_some(), eager: a write ref on a device that isn't rw is put
        (R::RW == READ || ca.mi.state as u32 == c::bch_member_state::BCH_MEMBER_STATE_rw.0).then_some(ca)
    }

    /// What @f makes of each device it doesn't skip: each step its own RCU
    /// section, resuming after the device the last one stopped at, as
    /// __bch2_next_dev().
    fn walk<'f, T>(&'f self, mut f: impl FnMut(&c::bch_dev) -> Option<T> + 'f) -> impl Iterator<Item = T> + 'f {
        let mut next = 0;
        core::iter::from_fn(move || {
            let rcu = read_lock();
            (next..self.nr_devices()).find_map(|dev| {
                next = dev + 1;
                self.dev_rcu(dev, &rcu).and_then(&mut f)
            })
        })
    }

    /// The filesystem's devices, a ref on each: for_each_member_device().
    pub fn members(&self) -> impl Iterator<Item = DevRef> + '_ {
        self.walk(|ca| Some(ca.get_ref()))
    }

    /// The devices in a state in @states (a mask of 1 << state), an io ref on
    /// each taken as @ref_idx, put when it's dropped: __for_each_online_member().
    pub fn io_members<R: IoRefIdx>(&self, states: u32, ref_idx: R) -> impl Iterator<Item = DevIoRef<'_, R>> {
        self.walk(move |ca| {
            (states & 1 << ca.mi.state != 0).then(|| DevIoRef::tryget(ca, ref_idx)).flatten()
        })
    }

    /// Every online device, an io ref for reading on each: for_each_online_member().
    pub fn online_members(&self, ref_idx: c::bch_dev_read_ref) -> impl Iterator<Item = DevIoRef<'_, c::bch_dev_read_ref>> {
        self.io_members(!0, ref_idx)
    }

    /// The rw devices, an io ref for writing on each: for_each_rw_member().
    pub fn rw_members(&self, ref_idx: c::bch_dev_write_ref) -> impl Iterator<Item = DevIoRef<'_, c::bch_dev_write_ref>> {
        self.io_members(1 << c::bch_member_state::BCH_MEMBER_STATE_rw.0, ref_idx)
    }

    /// The rw and ro devices, an io ref for reading on each: for_each_readable_member().
    pub fn readable_members(&self, ref_idx: c::bch_dev_read_ref)
        -> impl Iterator<Item = DevIoRef<'_, c::bch_dev_read_ref>>
    {
        self.io_members(1 << c::bch_member_state::BCH_MEMBER_STATE_rw.0 |
                        1 << c::bch_member_state::BCH_MEMBER_STATE_ro.0, ref_idx)
    }
}

// ── Missing devices and buckets, reported ────────────────────────────────

impl Fs {
    /// A pointer in @k to device @dev, which there isn't: said to @out,
    /// counted as the fsck error it is, and check_allocations scheduled to
    /// repair it - the error is scheduling's. Nothing for
    /// BCH_SB_MEMBER_INVALID, which names no device.
    pub fn dev_missing_bkey_msg(&self, k: BkeySC<'_>, dev: u32, out: &mut Printbuf) -> Result<(), BchError> {
        if dev == c::BCH_SB_MEMBER_INVALID {
            return Ok(());
        }

        let removed = self.devs_removed.test(dev as usize);
        crate::prt!(out, "pointer to {} device {dev} in key\n{}\n",
                    if removed { "removed" } else { "nonexistent" }, k.to_text(self));

        count_fsck_err(self, if removed { id::ptr_to_removed_device } else { id::ptr_to_invalid_device }, out);
        passes::run_explicit(self, out, c::bch_recovery_pass::BCH_RECOVERY_PASS_check_allocations,
                             c::bch_run_recovery_pass_flags::empty())
    }

    /// dev_missing_bkey_msg(), logged.
    pub fn dev_missing_bkey(&self, k: BkeySC<'_>, dev: u32) -> Result<(), BchError> {
        self.dev_missing_bkey_msg(k, dev, &mut LogMsg::new(self))
    }

    /// A pointer to device @dev, which there isn't, from where there's no key
    /// to show: logged, with a backtrace, as an inconsistency. Safe from
    /// atomic context - nothing it allocates can sleep.
    fn dev_missing_atomic(&self, dev: u32) {
        if dev == c::BCH_SB_MEMBER_INVALID {
            return;
        }

        let mut buf = Printbuf::new();
        let mut buf = buf.atomic();

        unsafe { crate::util::log::__bch2_log_msg_start(self.name().as_ptr().cast(), buf.as_raw()) };
        write!(buf, "pointer to {} device {dev}\n",
               if self.devs_removed.test(dev as usize) { "removed" } else { "nonexistent" });
        unsafe {
            c::bch2_prt_current_backtrace(buf.as_raw(), 1, c::GFP_ATOMIC);
            c::__bch2_inconsistent_error(self.as_ptr(), buf.as_raw());
        }
        print_str(self, LogLevel::ERR, &mut buf);
    }
}

#[c_export]
pub fn bch2_dev_missing_bkey_msg(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, dev: c_uint, out: &mut Printbuf) -> c_int {
    ret_to_c(Fs::from_c(c).dev_missing_bkey_msg(k, dev, out))
}

#[c_export]
pub fn bch2_dev_missing_bkey(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, dev: c_uint) -> c_int {
    ret_to_c(Fs::from_c(c).dev_missing_bkey(k, dev))
}

#[c_export]
pub fn bch2_dev_missing_atomic(c: &Opaque<c::bch_fs>, dev: c_uint) {
    Fs::from_c(c).dev_missing_atomic(dev)
}

impl c::bch_dev {
    /// A pointer to bucket @bucket, which this device hasn't: an
    /// inconsistency.
    pub fn bucket_missing(&self, bucket: u64) {
        crate::fs_inconsistent!(self.fs(), "pointer to nonexistent bucket {bucket} on device {} (valid range {}-{})",
                                self.name().to_str().unwrap_or("?"), self.mi.first_bucket, self.mi.nbuckets);
    }
}

#[c_export]
pub fn bch2_dev_bucket_missing(ca: &c::bch_dev, bucket: u64) {
    ca.bucket_missing(bucket)
}

// ── References ───────────────────────────────────────────────────────────

/// A ref on a device - ca->ref: it stays a member while this is held. Put
/// on drop; cloned with bch2_dev_get().
pub struct DevRef(*mut c::bch_dev);

impl DevRef {
    /// For C functions taking *mut bch_dev.
    pub fn as_mut_ptr(&self) -> *mut c::bch_dev {
        self.0
    }
}

impl core::ops::Deref for DevRef {
    type Target = c::bch_dev;

    fn deref(&self) -> &c::bch_dev {
        // SAFETY: the ref keeps it
        unsafe { &*self.0 }
    }
}

impl Clone for DevRef {
    fn clone(&self) -> Self {
        self.get_ref()
    }
}

impl Drop for DevRef {
    fn drop(&mut self) {
        unsafe { c::rust_bch2_dev_put(self.0) };
    }
}

/// ca->ref_outer: keeps the bch_dev's memory alive and nothing else - its
/// removal can complete while this is held. For contexts that block on
/// state_lock, where a DevRef mustn't be held: recheck ca.removing under the
/// lock before touching member state.
pub struct DevOuterRef(*mut c::bch_dev);

impl core::ops::Deref for DevOuterRef {
    type Target = c::bch_dev;

    fn deref(&self) -> &c::bch_dev {
        // SAFETY: the ref keeps the memory
        unsafe { &*self.0 }
    }
}

impl Drop for DevOuterRef {
    fn drop(&mut self) {
        unsafe { c::rust_bch2_dev_put_outer(self.0) };
    }
}

/// What an io ref is taken as: a read ref's index is a bch_dev_read_ref, a
/// write ref's a bch_dev_write_ref - so which of io_ref[] follows from it.
pub trait IoRefIdx: Copy + 'static {
    const RW: usize;
    fn idx(self) -> u32;
}

impl IoRefIdx for c::bch_dev_read_ref {
    const RW: usize = READ;
    fn idx(self) -> u32 { self as u32 }
}

impl IoRefIdx for c::bch_dev_write_ref {
    const RW: usize = WRITE;
    fn idx(self) -> u32 { self as u32 }
}

/// An io ref on a device - io_ref[R::RW] as @ref_idx: its block device is
/// open while this is held. Put on drop.
pub struct DevIoRef<'f, R: IoRefIdx> {
    ca:      &'f c::bch_dev,
    ref_idx: R,
}

impl<'f, R: IoRefIdx> DevIoRef<'f, R> {
    fn tryget(ca: &c::bch_dev, ref_idx: R) -> Option<Self> {
        // SAFETY: tryget only reads and counts
        unsafe { c::rust_enumerated_ref_tryget((&raw const ca.io_ref[R::RW]).cast_mut(), ref_idx.idx()) }
            // SAFETY: the io ref keeps it, past whatever section found it
            .then(|| DevIoRef { ca: unsafe { &*(ca as *const c::bch_dev) }, ref_idx })
    }
}

impl<R: IoRefIdx> core::ops::Deref for DevIoRef<'_, R> {
    type Target = c::bch_dev;

    fn deref(&self) -> &c::bch_dev {
        self.ca
    }
}

impl<R: IoRefIdx> Drop for DevIoRef<'_, R> {
    fn drop(&mut self) {
        unsafe { c::rust_enumerated_ref_put((&raw const self.ca.io_ref[R::RW]).cast_mut(), self.ref_idx.idx()) };
    }
}

/// The last device looked up, its ref kept while lookups keep hitting it:
/// bch2_dev_iterate(), for walks whose keys mostly stay on one device.
#[derive(Default)]
pub struct DevCache(Option<DevRef>);

impl DevCache {
    /// Device @dev, if there is one: bch2_dev_iterate_noerror().
    pub fn get(&mut self, fs: &Fs, dev: u32) -> Option<&DevRef> {
        if self.0.as_ref().is_none_or(|ca| u32::from(ca.dev_idx) != dev) {
            self.0 = fs.dev_get(dev);
        }
        self.0.as_ref()
    }

    /// get(), a missing device reported: bch2_dev_iterate().
    pub fn tryget(&mut self, fs: &Fs, dev: u32) -> Result<&DevRef, BchError> {
        self.get(fs, dev).ok_or_else(|| fs.dev_missing(dev))
    }
}

impl Fs {
    /// Whether device @dev has its block device open: bch2_dev_idx_is_online().
    pub fn dev_idx_is_online(&self, dev: u32) -> bool {
        self.devs_online.test(dev as usize)
    }

    /// bch2_dev_rotational().
    pub fn dev_rotational(&self, dev: u32) -> bool {
        dev != c::BCH_SB_MEMBER_INVALID && self.devs_rotational.test(dev as usize)
    }

    /// Whether @dev is missing, or being evacuated: bch2_dev_bad_or_evacuating_rcu().
    pub fn dev_bad_or_evacuating(&self, dev: u32, rcu: &ReadGuard) -> bool {
        self.dev_rcu(dev, rcu).is_none_or(|ca| {
            ca.mi.state as u32 == c::bch_member_state::BCH_MEMBER_STATE_evacuating.0
        })
    }
}

impl c::bch_dev {
    /// Its filesystem.
    pub fn fs(&self) -> &Fs {
        // SAFETY: a device's filesystem outlives it
        unsafe { Fs::borrow_raw(self.fs) }
    }

    /// ca->mi made @mi: see "In memory" above.
    fn set_mi(&self, _sb: &SbLockGuard, mi: c::bch_member_cpu) {
        // SAFETY: sb_lock, held, is ca->mi's writers' lock; its readers take
        // none, as in C
        unsafe { (&raw const self.mi).cast_mut().write(mi) }
    }

    /// Its IO errors, by kind: since the filesystem was made, and since they
    /// were last reset.
    pub fn io_errors_to_text(&self, out: &mut Printbuf) {
        let m = sb_member(self.fs().sb_lock().sb(), self.dev_idx.into()).unwrap_or_default();
        let errors = || MEMBER_ERROR_NAMES.iter().zip(&self.errors).map(|(n, e)| (n, e.load(Ordering::Relaxed) as u64));

        out.tabstop_push(12);

        write!(out, "IO errors since filesystem creation\n");
        {
            let mut out = out.indent(2);
            for (name, n) in errors() {
                write!(out, "{name}:\t{n}\n");
            }
        }

        let since = (ktime_get_real_seconds() as u64).wrapping_sub(m.errors_reset_time.get());
        write!(out, "IO errors since ");
        out.time_units(since.wrapping_mul(NSEC_PER_SEC));
        write!(out, " ago\n");
        {
            let mut out = out.indent(2);
            for ((name, n), at_reset) in errors().zip(&m.errors_at_reset) {
                write!(out, "{name}:\t{}\n", n.wrapping_sub(at_reset.get()));
            }
        }

        if m.flush_errors.get() != 0 {
            write!(out, "Flush errors (device not honoring FUA/flush):\t{}\n", m.flush_errors.get());
        }
    }

    /// Start counting IO errors again from now, in the superblock.
    pub fn errors_reset(&self) {
        let fs = self.fs();
        let mut sb = fs.sb_lock();
        let m = sb.fs_member_mut(self.dev_idx.into());

        for (r, e) in m.errors_at_reset.iter_mut().zip(&self.errors) {
            *r = (e.load(Ordering::Relaxed) as u64).into();
        }
        m.errors_reset_time = (ktime_get_real_seconds() as u64).into();

        fs.write_super(&mut sb);
    }

    /// The sectors read and written, by data type: every CPU's count summed.
    pub fn io_done(&self) -> c::io_count {
        let mut ret = c::io_count::default();
        let src: *const u64 = self.io_done.cast();
        for (i, row) in ret.sectors.iter_mut().enumerate() {
            let at = i * row.len();
            // SAFETY: ca->io_done is a per-CPU io_count, live as long as the
            // device; row @i starts @at u64s into it
            unsafe { acc_u64s_percpu(row, src.add(at)) };
        }
        ret
    }

    /// Its block device's name, model and serial, and whether it's rotational.
    fn identity(&self) -> c::bch_dev_identity {
        let mut id = c::bch_dev_identity::default();
        let bdev = self.disk_sb.bdev;
        unsafe {
            c::bch2_dev_read_identity(bdev,
                                      id.name.as_mut_ptr(), id.name.len(),
                                      id.model.as_mut_ptr(), id.model.len(),
                                      id.serial.as_mut_ptr(), id.serial.len());
            id.rotational = c::bch2_bdev_rot(bdev);
        }
        id
    }

    /// Its identity, recorded in its member.
    pub fn mi_field_upgrades(&self) {
        let id = self.identity();

        let fs = self.fs();
        let mut sb = fs.sb_lock();
        if mi_field_upgrades(&mut sb, self.dev_idx.into(), &id) {
            fs.write_super(&mut sb);
        }
    }

    /// The latest IO's latency, for reads (READ) or writes (WRITE).
    pub fn cur_latency(&self, rw: usize) -> i64 {
        self.cur_latency[rw].load(Ordering::Relaxed)
    }

    /// The IO refs held, by kind - for reads (READ) or writes (WRITE).
    pub fn io_refs_to_text(&self, out: &mut Printbuf, rw: usize) {
        let names = match rw {
            READ => unsafe { c::bch2_dev_read_refs.as_ptr() },
            _    => unsafe { c::bch2_dev_write_refs.as_ptr() },
        };
        // SAFETY: the names of io_ref[rw]'s kinds
        unsafe { self.io_ref[rw].to_text(out, names) }
    }

    /// The data types with data on it, by the superblock's replicas: a mask
    /// of bch_data_types.
    pub fn has_data(&self) -> u32 {
        self.fs().sb_lock().sb().dev_has_data(self.dev_idx.into())
    }

    /// A ref on it - bch2_dev_get(): the device is alive, being in hand, so
    /// the get can't fail.
    pub fn get_ref(&self) -> DevRef {
        let ca = (self as *const c::bch_dev).cast_mut();
        unsafe { c::rust_bch2_dev_get(ca) };
        DevRef(ca)
    }

    /// An outer ref on it - bch2_dev_get_outer().
    pub fn outer_ref(&self) -> DevOuterRef {
        let ca = (self as *const c::bch_dev).cast_mut();
        unsafe { c::rust_bch2_dev_get_outer(ca) };
        DevOuterRef(ca)
    }

    /// Whether @b is one of its buckets: bucket_valid().
    pub fn bucket_valid(&self, b: u64) -> bool {
        b.wrapping_sub(self.mi.first_bucket.into()) < self.mi.nbuckets_minus_first
    }

    /// The block device's name - sdX and the like: up to its NUL, within the
    /// buffer, or empty if it has none.
    pub fn name(&self) -> &CStr {
        // SAFETY: a c_char is a byte: the buffer's bytes
        let bytes = unsafe { core::slice::from_raw_parts(self.name.as_ptr().cast::<u8>(), self.name.len()) };
        CStr::from_bytes_until_nul(bytes).unwrap_or(c"")
    }

    /// Whether its block device is open for IO: bch2_dev_is_online().
    pub fn is_online(&self) -> bool {
        // SAFETY: only read
        !unsafe { c::rust_enumerated_ref_is_zero((&raw const self.io_ref[READ]).cast_mut()) }
    }

    /// Whether the allocator writes to it: bch2_dev_is_rw().
    pub fn is_rw(&self) -> bool {
        // SAFETY: a device's filesystem outlives it
        let fs = unsafe { &*self.fs };
        fs.allocator.rw_devs[c::bch_data_type::BCH_DATA_free.0 as usize].test(self.dev_idx.into())
    }

    /// What a new copy written to it would be worth - nothing unless it's rw:
    /// bch2_dev_rw_durability().
    pub fn rw_durability(&self) -> u32 {
        if self.is_rw() { self.mi.durability.into() } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::vstructs::plain_bytes_mut;

    fn new_sb(nr: u8) -> c::bch_sb_handle {
        let mut sb = c::bch_sb_handle::default();
        sb.sb_realloc(0).unwrap();
        sb.sb_mut().nr_devices = nr;
        sb
    }

    fn member(i: u8) -> c::bch_member {
        let mut m = c::bch_member::default();
        m.uuid.b = [i + 1; 16];
        m.nbuckets = (1000 * u64::from(i) + 1).into();
        m
    }

    /// @nr members, each its first @bytes, at a stride of @bytes - as an
    /// older version wrote them.
    fn put_v2(sb: &mut c::bch_sb_handle, nr: u8, bytes: usize) {
        let u64s = (MEMBERS_V2_HEAD + bytes * usize::from(nr)).div_ceil(8) as u32;
        let mi = sb.field_resize::<c::bch_sb_field_members_v2>(u64s).unwrap();
        mi.member_bytes = (bytes as u16).into();
        let mut m = members_v2_mut(mi).unwrap();
        for i in 0..nr {
            m.slot_bytes_mut(i.into()).unwrap().copy_from_slice(&plain_bytes(&member(i))[..bytes]);
        }
    }

    fn whole(sb: &mut c::bch_sb_handle, i: u8) -> c::bch_member {
        *sb.member_mut(i.into()).expect("a whole member")
    }

    /// From members_v1: each member as it was, zero past the 56 bytes v1 has.
    #[test]
    fn members_v2_from_v1() {
        let mut sb = new_sb(3);
        let u64s = (MEMBERS_V1_HEAD + BCH_MEMBER_V1_BYTES * 3).div_ceil(8) as u32;
        sb.field_resize::<c::bch_sb_field_members_v1>(u64s).unwrap();
        for i in 0..3 {
            sb.put_member_v1::<c::bch_sb_field_members_v1>(MEMBERS_V1_HEAD, i.into(), &member(i));
        }

        sb.members_v2_init().unwrap();
        let mi = sb.field::<c::bch_sb_field_members_v2>().unwrap();
        assert_eq!(usize::from(mi.member_bytes.get()), size_of::<c::bch_member>());
        for i in 0..3 {
            assert_eq!(plain_bytes(&whole(&mut sb, i)), plain_bytes(&member(i)));
        }
    }

    /// Grown in place: each member moved whole, and the bytes past what it
    /// had - the next member's, before - zero.
    #[test]
    fn members_v2_grown() {
        let mut sb = new_sb(3);
        put_v2(&mut sb, 3, BCH_MEMBER_V1_BYTES);
        assert!(sb.member_mut(0).is_none());

        sb.members_v2_init().unwrap();
        for i in 0..3 {
            let mut want = c::bch_member::default();
            plain_bytes_mut(&mut want)[..BCH_MEMBER_V1_BYTES]
                .copy_from_slice(&plain_bytes(&member(i))[..BCH_MEMBER_V1_BYTES]);
            assert_eq!(plain_bytes(&whole(&mut sb, i)), plain_bytes(&want));
        }
    }

    /// members_v1's to_text counts its members by its own stride: C's used
    /// bch_member's, so printed a few and said nr_devices didn't match.
    #[test]
    fn members_v1_to_text() {
        let mut sb = new_sb(3);
        let u64s = (MEMBERS_V1_HEAD + BCH_MEMBER_V1_BYTES * 3).div_ceil(8) as u32;
        sb.field_resize::<c::bch_sb_field_members_v1>(u64s).unwrap();
        for i in 0..3 {
            sb.put_member_v1::<c::bch_sb_field_members_v1>(MEMBERS_V1_HEAD, i.into(), &member(i));
        }

        let mut out = Printbuf::new();
        let f = sb.field::<c::bch_sb_field_members_v1>().unwrap();
        members_to_text(&mut out, sb.sb(), members_in(f.as_bytes(), MEMBERS_V1_HEAD, BCH_MEMBER_V1_BYTES));
        assert!(!out.as_str().contains("mismatch"));
        assert_eq!(out.as_str().matches("Device:").count(), 3);
    }

    /// The mirror is each member's first 56 bytes - and, past extent_flags,
    /// gone: versions that read it can't read the superblock.
    #[test]
    fn members_v1_mirror() {
        let mut sb = new_sb(2);
        put_v2(&mut sb, 2, size_of::<c::bch_member>());
        sb.members_cpy_v2_v1().unwrap();
        for i in 0..2 {
            let m = members_v1(sb.sb()).unwrap().get(i.into()).unwrap();
            assert_eq!(plain_bytes(&m)[..BCH_MEMBER_V1_BYTES], plain_bytes(&member(i))[..BCH_MEMBER_V1_BYTES]);
        }

        sb.sb_mut().set_version_incompat(c::bcachefs_metadata_version::extent_flags.0 as u64 + 1);
        sb.members_cpy_v2_v1().unwrap();
        assert!(sb.field::<c::bch_sb_field_members_v1>().is_none());
    }

    /// A range's bits, then coarsened, are the bits it has at the coarser
    /// shift: what's marked stays marked, and only the bits covering it.
    #[test]
    fn btree_bitmap_bits() {
        assert_eq!(bitmap_bits(3, 9, 2), 0b111);
        assert_eq!(bitmap_bits(4, 8, 2), 0b10);
        assert_eq!(bitmap_coarsen(0b1100_0001, 1), 0b1001);

        for (start, end) in [(0, 1), (3, 9), (4, 8), (100, 1000), (63 << 4, 64 << 4)] {
            for by in 1..4 {
                assert_eq!(bitmap_coarsen(bitmap_bits(start, end, 4), by), bitmap_bits(start, end, 4 + by as u8));
            }
        }
    }
}
