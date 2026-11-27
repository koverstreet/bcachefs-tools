#![allow(non_camel_case_types)]

use crate::init::passes::{RecoveryPass, RunRecoveryPassFlags};
use crate::fs::{Dev, Fs, SbLockGuard};
use crate::btree::bkey::{BkeySC, BkeyV, POS_MIN, ValidateFlags};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeNodeIter};
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::c;
use crate::init::dev::{DevsList, DevsMask, bch_sb_buf, DevFlag};
use crate::sb::members_format::{Member, BCH_MEMBER_NBUCKETS_MAX, BCH_MI_BTREE_BITMAP_SHIFT_MAX,
    BCH_MIN_NR_NBUCKETS, SbFieldMembersV1, SbFieldMembersV2, BCH_SB_MEMBER_INVALID,
    BCH_SB_MEMBERS_MAX, IopsMeasurement, MemberErrorType};
use crate::errcode::{Errcode, BchError};
use crate::locking::MemallocFlags;
use crate::{prt, sb_err_on};
use crate::os::kernel::{ktime_get_real_seconds, nsecs_to_jiffies, NSEC_PER_SEC, READ, WRITE};
use crate::os::workqueue::Queue;
use crate::util::backtrace::prt_current_backtrace;
use crate::util::{Bytes, DateTime, HumanReadable, Nsecs, Printbuf, Sectors, ToText};
use crate::util::log::{print_str, LogLevel, LogMsg};
use crate::init::error::{count_fsck_err, id};
use crate::init::passes;
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::place::Placed;
use crate::util::refcount::{AtomicRef, PercpuRef, RefGuard, TryRefcount};
use crate::alloc::disk_groups;
use crate::util::ffi::c_char;
use crate::data_structures::strided::{Strided, StridedMut};
use crate::util::vstructs::{bytes_of_mut, plain_bytes, Head, InPlace, Unsized, Unvalidated, Validated};
use core::ffi::CStr;
use core::marker::PhantomData;
use core::sync::atomic::Ordering;
use cstruct_macros::{bch_enum, MaybeZeroable, TypeInfo, VolatileValue};
use crate::util::projections::TearSafe;
use super::io::*;
use crate::format::{Bpos, DataType, DataTypes, MemberInitialized, MemberState, MetadataVersion, SbFeatures,
    Sb, BTREE_MAX_DEPTH, SbFieldType};

#[bch_enum(closed)]
#[nr]
#[repr(u32)]
pub enum DevReadRefKind {
    rust,
    bch2_online_devs,
    read_fua_test,
    journal_read,
    fs_journal_alloc,
    fs_resize_on_mount,
    btree_node_read,
    btree_node_scrub,
    btree_node_write,
    btree_node_scan,
    btree_verify_replicas,
    io_read,
}

#[bch_enum(closed)]
#[nr]
#[repr(u32)]
pub enum DevWriteRefKind {
    rust,
    journal_write,
    journal_discard,
    discard_bucket,
    discard_one_bucket_fast,
    do_invalidates,
    nocow_flush,
    io_write,
    ec_bucket_zero,
}


#[repr(C)]
#[derive(Clone, Copy, MaybeZeroable, TypeInfo)]
pub struct DevIdentity {
    pub name: [c_char; 16 + 1],
    pub model: [c_char; 64 + 1],
    pub serial: [c_char; 64 + 1],
    pub rotational: bool,
}

// ca->mi: read by anyone, a field at a time - a copy half made is one
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, VolatileValue, TypeInfo)]
pub struct MemberCpu {
    pub nbuckets: u64, /* device size */
    pub nbuckets_minus_first: u64,
    pub first_bucket: u16, /* index of first bucket used */
    pub bucket_size: u16, /* sectors */
    pub failure_domain: u16,
    pub data_allowed: DataTypes,
    pub state: MemberState,
    pub discard: bool,
    pub durability: u8,
    pub freespace_initialized: bool,
    pub initialized: MemberInitialized,
    pub resize_on_mount: bool,
    pub rotational: bool,
    pub btree_bitmap_shift: u8,
    pub btree_allocated_bitmap: u64,
}

// ---------------------------------------------------------------------------
// Members — bounds-checked readers for SbFieldMembersV2 and v1
//
// Members are variable-size: each entry is `member_bytes` wide, which may
// be smaller than `sizeof(Member)` for on-disk backward compatibility.
// The reader copies into a zeroed struct;
// to edit one, SbHandle::member_mut(), for a member that's all there.
// ---------------------------------------------------------------------------

const BCH_MEMBER_V1_BYTES: usize = crate::sb::members_format::BCH_MEMBER_V1_BYTES as usize;
const MEMBERS_V2_HEAD: usize = size_of::<Head<SbFieldMembersV2>>();
const MEMBERS_V1_HEAD: usize = size_of::<Head<SbFieldMembersV1>>();

/// BCH_SB_MEMBER_DELETED_UUID.
pub(crate) const MEMBER_DELETED_UUID: [u8; 16] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

impl Member {
    /// Not an empty slot or a deleted member.
    pub fn alive(&self) -> bool {
        self.uuid.b != [0; 16] && !self.deleted()
    }

    /// Removed: its slot isn't free until fsck clears it.
    pub fn deleted(&self) -> bool {
        self.uuid.b == MEMBER_DELETED_UUID
    }
}

pub fn member_exists<S: SbState>(sb: &Sb<S>, dev: u32) -> bool {
    dev < u32::from(sb.nr_devices) && sb_member(sb, dev).is_some_and(|m| m.alive())
}

impl<S: SbState> Sb<S> {
    /// Its members that exist, of nr_devices' slots.
    pub fn number_of_devices(&self) -> u32 {
        (0..u32::from(self.nr_devices)).filter(|&i| member_exists(self, i)).count() as u32
    }
}

/// A free member slot: past the last one in use if there's room, else the
/// empty one least recently mounted - never BCH_SB_MEMBER_INVALID, the
/// sentinel. Deleted ones aren't free until fsck clears them.
fn sb_member_find_slot(fs: &Fs, sb: &Sb) -> Result<u32, BchError> {
    let nr = fs.nr_devices();
    if nr < BCH_SB_MEMBERS_MAX && nr != BCH_SB_MEMBER_INVALID {
        return Ok(nr);
    }

    let mut best: Option<(u64, u32)> = None;
    let mut nr_deleted = 0;

    for dev in (0..BCH_SB_MEMBERS_MAX).filter(|&i| i != BCH_SB_MEMBER_INVALID) {
        let m = sb_member(sb, dev).unwrap_or_else(::pin_init::zeroed);
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
    Err(Errcode::ENOSPC_sb_members.into())
}

/// A member slot for a new device, members_v2 and nr_devices grown to hold
/// it.
pub(crate) fn sb_member_alloc(fs: &Fs, sb: &mut SbLockGuard<'_>) -> Result<u32, BchError> {
    let dev = sb_member_find_slot(fs, sb.sb())?;
    let nr = (dev + 1).max(fs.nr_devices());

    let stride = usize::from(sb.field::<SbFieldMembersV2>().expect("members_v2").member_bytes.get());
    let u64s = (MEMBERS_V2_HEAD + stride * nr as usize).div_ceil(size_of::<u64>());

    fs.sb_resize(sb).field_resize::<SbFieldMembersV2>(u64s as u32)
        .ok_or(BchError::from(Errcode::ENOSPC_sb_members))?;
    sb.sb_mut().nr_devices = nr as u8;
    Ok(dev)
}

impl Fs {
    /// Deleted members' slots made free: fsck's, once nothing points to them.
    pub(crate) fn sb_members_clean_deleted(&self) {
        let mut sb = self.sb_lock();
        let mut dirty = false;

        for i in 0..self.nr_devices() {
            let m = sb.fs_member_mut(i);
            if m.deleted() {
                m.uuid.b = [0; 16];
                dirty = true;
            }
        }
        if dirty {
            self.write_super(&mut sb);
        }
    }
}

/// From members_v2 if the superblock has it, else members_v1.
pub fn sb_member<S: SbState>(sb: &Sb<S>, idx: u32) -> Option<Member> {
    match members_v2(sb) {
        Some(v2) => v2.get(idx),
        None     => members_v1(sb)?.get(idx),
    }
}

/// Read-only view of members_v2 with bounds-checked access.
pub struct MembersV2<'a> {
    members:    Strided<'a, Member>,
    nr_devices: u32,
}

impl<'a> MembersV2<'a> {
    pub fn get(&self, idx: u32) -> Option<Member> {
        (idx < self.nr_devices).then(|| self.members.get(idx as usize))?.map(|m| m.get())
    }

    pub fn member_bytes(&self) -> usize {
        self.members.stride()
    }

    pub fn nr_devices(&self) -> u32 {
        self.nr_devices
    }

    pub fn iter(&self) -> impl Iterator<Item = Member> + '_ {
        (0..self.nr_devices).filter_map(|i| self.get(i))
    }
}

/// Read-only view of members_v1 with bounds-checked access.
pub struct MembersV1<'a> {
    members:    Strided<'a, Member>,
    nr_devices: u32,
}

impl<'a> MembersV1<'a> {
    pub fn get(&self, idx: u32) -> Option<Member> {
        (idx < self.nr_devices).then(|| self.members.get(idx as usize))?.map(|m| m.get())
    }

    pub fn nr_devices(&self) -> u32 {
        self.nr_devices
    }

    pub fn iter(&self) -> impl Iterator<Item = Member> + '_ {
        (0..self.nr_devices).filter_map(|i| self.get(i))
    }
}

impl<S: InPlace> SbFieldMembersV2<S> {
    fn members(&self) -> Option<Strided<'_, Member>> {
        Strided::new(&self._members, self.member_bytes.get().into())
    }
}

impl<S: InPlace> SbFieldMembersV1<S> {
    fn members(&self) -> Option<Strided<'_, Member>> {
        Strided::new(&self._members, BCH_MEMBER_V1_BYTES)
    }
}

/// Construct a MembersV2 reader from a superblock.
pub fn members_v2<S: SbState>(sb: &Sb<S>) -> Option<MembersV2<'_>> {
    Some(MembersV2 {
        members: S::field::<SbFieldMembersV2>(sb)?.members()?,
        nr_devices: sb.nr_devices.into(),
    })
}

/// Construct a MembersV1 reader from a superblock.
pub fn members_v1<S: SbState>(sb: &Sb<S>) -> Option<MembersV1<'_>> {
    Some(MembersV1 {
        members: S::field::<SbFieldMembersV1>(sb)?.members()?,
        nr_devices: sb.nr_devices.into(),
    })
}

fn members_v2_mut(mi: &mut SbFieldMembersV2<Validated>) -> Option<StridedMut<'_, Member>> {
    let stride = mi.member_bytes.get().into();
    StridedMut::new(&mut mi._members, stride)
}

impl bch_sb_buf {
    /// Member @idx, to edit: None if there's no such member in the section,
    /// or it's shorter than a Member - written before it grew.
    pub fn member_mut(&mut self, idx: u32) -> Option<&mut Member> {
        if idx >= u32::from(self.sb().nr_devices) {
            return None;
        }
        members_v2_mut(self.field_mut::<SbFieldMembersV2>()?)?.into_whole_mut(idx as usize)
    }

    fn put_member_v1<F: SbSection<Arm: Unsized> + ?Sized>(&mut self, start: usize, i: u32, m: &Member) {
        let at = start + i as usize * BCH_MEMBER_V1_BYTES;
        let f = bytes_of_mut(self.field_mut::<F>().expect("members: just sized"));
        f[at..at + BCH_MEMBER_V1_BYTES].copy_from_slice(&plain_bytes(m)[..BCH_MEMBER_V1_BYTES]);
    }
}

/// A superblock's members resized: which superblock it is says what grows
/// with it - the filesystem's, its devices'.
pub trait SbMembersMut: SbResize {
    /// members_v1, the mirror of members_v2 that versions before
    /// extent_flags read - or none, for a superblock they can't.
    fn members_cpy_v2_v1(&mut self) -> Result<(), BchError> {
        if u32::from(self.sb().version_incompat()) > MetadataVersion::extent_flags.0 {
            self.field_delete(SbFieldType::members_v1);
            return Ok(());
        }

        let nr = u32::from(self.sb().nr_devices);
        let u64s = (MEMBERS_V1_HEAD + BCH_MEMBER_V1_BYTES * nr as usize).div_ceil(size_of::<u64>());
        self.field_resize::<SbFieldMembersV1>(u64s as u32)
            .ok_or(BchError::from(Errcode::ENOSPC_sb_members))?;

        for i in 0..nr {
            let m = members_v2(self.sb()).and_then(|v2| v2.get(i)).expect("members_v2: every member");
            self.put_member_v1::<SbFieldMembersV1>(MEMBERS_V1_HEAD, i, &m);
        }
        Ok(())
    }

    /// After, every member of members_v2 is whole. None if there isn't room.
    fn members_v2_init(&mut self) -> Option<()> {
        let nr = u32::from(self.sb().nr_devices);
        let member = size_of::<Member>();
        let u64s = ((MEMBERS_V2_HEAD + member * nr as usize).div_ceil(size_of::<u64>())) as u32;

        if self.field::<SbFieldMembersV2>().is_none() {
            let mi = self.field_resize::<SbFieldMembersV2>(u64s)?;
            mi.pad = Default::default();
            mi.member_bytes = (BCH_MEMBER_V1_BYTES as u16).into();

            for i in 0..nr {
                let m = members_v1(self.sb()).and_then(|v1| v1.get(i)).unwrap_or_else(::pin_init::zeroed);
                self.put_member_v1::<SbFieldMembersV2>(MEMBERS_V2_HEAD, i, &m);
            }
        }

        let stride: usize = self.field::<SbFieldMembersV2>()?.member_bytes.get().into();
        if stride < member {
            let mi = self.field_resize::<SbFieldMembersV2>(u64s)?;
            StridedMut::<Member>::grow(&mut bytes_of_mut(mi)[MEMBERS_V2_HEAD..], nr as usize, stride, member)
                .expect("members_v2: validated, and just sized");
            mi.member_bytes = (member as u16).into();
        }
        Some(())
    }
}

impl<T: SbResize + ?Sized> SbMembersMut for T {}

// ── Validating, and printing ─────────────────────────────────────────────

/// By value, as a member's arrays of them are.
const MEMBER_ERROR_NAMES: &[&str] = crate::names_by_value!(MemberErrorType);
const IOPS_NAMES: &[&str] = crate::names_by_value!(IopsMeasurement);

/// A __nonstring field: NUL terminated only if shorter than the field.
fn nonstring(b: &[u8]) -> &[u8] {
    &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]
}

fn invalid() -> Result<(), BchError> {
    Err(Errcode::invalid_sb_members.into())
}

fn validate_member(err: &mut Printbuf, m: &Member, sb: &Sb<Unvalidated>, i: u32) -> Result<(), BchError> {
    let nbuckets = m.nbuckets.get();
    sb_err_on!(err, nbuckets > BCH_MEMBER_NBUCKETS_MAX.into(), invalid_sb_members,
               "device {i}: too many buckets (got {nbuckets}, max {})", BCH_MEMBER_NBUCKETS_MAX)?;

    // wrapping, as C's: a first_bucket past nbuckets isn't caught here
    let usable = nbuckets.wrapping_sub(m.first_bucket.get().into());
    sb_err_on!(err, usable < BCH_MIN_NR_NBUCKETS.into(), invalid_sb_members,
               "device {i}: not enough buckets (got {usable}, min {})", BCH_MIN_NR_NBUCKETS)?;

    let (bucket_size, block_size) = (m.bucket_size.get(), sb.block_size.get());
    sb_err_on!(err, bucket_size < block_size, invalid_sb_members,
               "device {i}: bucket size {bucket_size} smaller than block size {block_size}")?;

    sb_err_on!(err, bucket_size < sb.btree_node_size(), invalid_sb_members,
               "device {i}: bucket size {bucket_size} smaller than btree node size {}",
               sb.btree_node_size())?;

    sb_err_on!(err, u32::from(m.btree_bitmap_shift) >= BCH_MI_BTREE_BITMAP_SHIFT_MAX,
               invalid_sb_members, "device {i}: invalid btree_bitmap_shift {}",
               m.btree_bitmap_shift)?;

    let no_alloc_info = sb.features.get().contains(SbFeatures::no_alloc_info);
    sb_err_on!(err, m.freespace_initialized() && no_alloc_info, invalid_sb_members,
               "device {i}: freespace initialized but fs has no alloc info")?;
    Ok(())
}

fn members_validate(sb: &Sb<Unvalidated>, members: Option<Strided<'_, Member>>,
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
fn members_to_text<S: SbState>(out: &mut Printbuf, sb: &Sb<S>, members: Option<Strided<'_, Member>>) {
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

impl SbFieldOps for SbFieldMembersV1<Validated> {
    fn validate(sb: &Sb<Unvalidated>, f: &SbFieldMembersV1<Unvalidated>,
                _flags: ValidateFlags, err: &mut Printbuf) -> Result<(), BchError> {
        members_validate(sb, f.members(), err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, sb: &Sb<Unvalidated>,
                f: &SbFieldMembersV1<Unvalidated>) {
        members_to_text(out, sb, f.members());
    }
}

impl SbFieldOps for SbFieldMembersV2<Validated> {
    fn validate(sb: &Sb<Unvalidated>, f: &SbFieldMembersV2<Unvalidated>,
                _flags: ValidateFlags, err: &mut Printbuf) -> Result<(), BchError> {
        members_validate(sb, f.members(), err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, sb: &Sb<Unvalidated>,
                f: &SbFieldMembersV2<Unvalidated>) {
        members_to_text(out, sb, f.members());
    }

    /// ca->mi, and the masks of rotational and removed devices.
    fn to_cpu(fs: &Fs, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        for ca in fs.members() {
            let dev = u32::from(ca.dev_idx);
            let m = sb_member(sb.sb(), dev).unwrap_or_else(::pin_init::zeroed);
            let mut mi = m.to_cpu();
            mi.failure_domain = failure_domain_id(sb.sb(), dev, &m);

            ca.set_mi(sb, mi);
            fs.devs_rotational.assign(dev as usize, mi.rotational);
        }

        if let Some(mi2) = members_v2(sb.sb()) {
            for i in 0..fs.nr_devices() {
                fs.devs_removed.assign(i as usize, mi2.get(i).unwrap_or_else(::pin_init::zeroed).deleted());
            }
        }
        Ok(())
    }

    /// The devices' error counts - memory's, counted by io_error().
    fn from_cpu(fs: &Fs, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        let rcu = read_lock();
        for ca in fs.devs_rcu(&rcu) {
            let m = sb.fs_member_mut(ca.dev_idx.into());
            for (d, e) in m.errors.iter_mut().zip(&ca.errors) {
                *d = (e.load(Ordering::Relaxed) as u64).into();
            }
        }
        Ok(())
    }
}

pub(crate) fn data_types_to_text(out: &mut Printbuf, types: DataTypes) {
    match types {
        t if !t.is_empty() => t.to_text(out),
        _                  => write!(out, "(none)"),
    }
}

fn label_to_text<S: SbState>(out: &mut Printbuf, m: &Member, sb: &Sb<S>) {
    write!(out, "Label:\t");
    match m.group() {
        0 => write!(out, "(none)"),
        g => disk_groups::disk_path_to_text_sb(out, sb, g as u32 - 1),
    }
    out.newline();
}

fn failure_domain_to_text(out: &mut Printbuf, m: &Member) {
    if m.failure_domain[0] != 0 {
        write!(out, "Failure domain:\t");
        out.write_bytes(nonstring(&m.failure_domain));
        out.newline();
    }
}

fn has_data_to_text<S: SbState>(out: &mut Printbuf, sb: &Sb<S>, idx: u32) {
    write!(out, "Has data:\t");
    data_types_to_text(out, sb.dev_has_data(idx));
    out.newline();
}

/// show-super's member: every field.
pub fn member_to_text<S: SbState>(out: &mut Printbuf, m: &Member, sb: &Sb<S>, idx: u32) {
    let bucket_size = u64::from(m.bucket_size.get());

    label_to_text(out, m, sb);
    failure_domain_to_text(out, m);
    prt!(out, "UUID:\t{}\n", m.uuid);
    prt!(out, "Size:\t{}\n", Sectors(m.nbuckets.get() * bucket_size));

    for (name, e) in MEMBER_ERROR_NAMES.iter().zip(&m.errors) {
        write!(out, "{name} errors:\t{}\n", e.get());
    }
    for (name, iops) in IOPS_NAMES.iter().zip(&m.iops) {
        write!(out, "{name} iops:\t{}\n", iops.get());
    }

    prt!(out, "Bucket size:\t{}\n", Sectors(bucket_size));
    write!(out, "First bucket:\t{}\n", m.first_bucket.get());
    write!(out, "Buckets:\t{}\n", m.nbuckets.get());

    match m.last_mount.get() {
        0 => prt!(out, "Last mount:\t(never)\n"),
        t => prt!(out, "Last mount:\t{}\n", DateTime(t as i64)),
    }
    write!(out, "Last superblock write:\t{}\n", m.seq.get());

    write!(out, "State:\t{}\n", m.state());
    write!(out, "Data allowed:\t");
    data_types_to_text(out, m.data_allowed());
    out.newline();
    has_data_to_text(out, sb, idx);
    write!(out, "Rotational:\t{}\n", m.rotational() as u8);

    match m.btree_bitmap_shift {
        s if s < 64 => prt!(out, "Btree allocated bitmap blocksize:\t{}\n", Bytes(1 << s)),
        s           => prt!(out, "Btree allocated bitmap blocksize:\t(invalid shift {s})\n"),
    }
    write!(out, "Btree allocated bitmap:\t{:064b}", m.btree_allocated_bitmap.get());
    out.newline();

    write!(out, "Durability:\t{}\n", m.durability().checked_sub(1).unwrap_or(1));
    write!(out, "Discard:\t{}\n", m.discard() as u8);
    write!(out, "Freespace initialized:\t{}\n", m.freespace_initialized() as u8);
    write!(out, "Resize on mount:\t{}\n", m.resize_on_mount() as u8);
    write!(out, "Unpark on online:\t{}\n", m.unpark_on_online() as u8);

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
fn member_to_text_short<S: SbState>(out: &mut Printbuf, m: &Member, sb: &Sb<S>, idx: u32) {
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

    write!(out, "State:\t{}\n", m.state());
    has_data_to_text(out, sb, idx);
}

pub(crate) fn dev_to_text_short(out: &mut Printbuf, sb: &Sb, dev: u32) {
    member_to_text_short(out, &sb_member(sb, dev).unwrap_or_else(::pin_init::zeroed), sb, dev);
}

/// Each of @devs that's a device of @fs's, short.
pub(crate) fn devs_mask_to_text(out: &mut Printbuf, fs: &Fs, sb: &Sb, devs: &DevsMask) {
    for dev in devs.iter() {
        if fs.dev_rcu(dev as u32, &read_lock()).is_some() {
            dev_to_text_short(out, sb, dev as u32);
        }
    }
}

// ── In memory: ca->mi, and what goes back ────────────────────────────────
//
// ca->mi is the superblock's member, decoded: written under sb_lock - set_mi()
// takes the guard - and read without it, as C's readers always have, so a
// reader can see a copy half made.

impl Member {
    /// As ca->mi has it - all but the failure domain, which takes the other
    /// members to number.
    pub fn to_cpu(&self) -> MemberCpu {
        let nbuckets = self.nbuckets.get();
        let first_bucket = self.first_bucket.get();

        MemberCpu {
            nbuckets,
            nbuckets_minus_first:   nbuckets.wrapping_sub(first_bucket.into()),
            first_bucket,
            bucket_size:            self.bucket_size.get(),
            failure_domain:         0,
            state:                  self.state(),
            discard:                self.discard(),
            data_allowed:           self.data_allowed(),
            // one-biased on disk: 0 is the default, 1
            durability:             self.durability().checked_sub(1).unwrap_or(1),
            freespace_initialized:  self.freespace_initialized(),
            initialized:            self.initialized(),
            resize_on_mount:        self.resize_on_mount(),
            rotational:             self.rotational(),
            btree_bitmap_shift:     self.btree_bitmap_shift,
            btree_allocated_bitmap: self.btree_allocated_bitmap.get(),
        }
    }
}

/// Member @dev's failure domain, as an id: one more than the first member
/// with the same string, 0 for none. Only compared for equality, so it
/// needn't be dense.
fn failure_domain_id(sb: &Sb, dev: u32, m: &Member) -> u16 {
    if m.failure_domain[0] == 0 {
        return 0;
    }
    (0..=dev).find(|&i| sb_member(sb, i).unwrap_or_else(::pin_init::zeroed).failure_domain == m.failure_domain)
        .map_or(0, |i| i as u16 + 1)
}

impl Fs {
    /// The worst IO latency seen, reading or writing as @rw, of @devs, in
    /// jiffies: to scale timeouts and "held too long" warnings so slow storage
    /// doesn't trip them.
    pub fn dev_latency_max(&self, devs: &DevsMask, rw: i32) -> usize {
        let rcu = read_lock();
        let ns = self.devs_rcu_in(devs.iter(), &rcu)
            .map(|ca| ca.io_latency.get(rw as usize).stats().stats.lock.lock().max_duration)
            .max()
            .unwrap_or(0);
        nsecs_to_jiffies(ns)
    }
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
pub(crate) fn mi_field_upgrades(sb: &mut SbLockGuard<'_>, dev: u32, id: &DevIdentity) -> bool {
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

/// Every online device's identity, recorded.
pub fn fs_mi_field_upgrades(fs: &Fs) {
    for ca in fs.online_members() {
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

impl Dev {
    /// Whether all of [@start, @start + @sectors) is marked in its member's
    /// bitmap - and, @with_gc, in the one gc is rebuilding. members.h has
    /// C's, an inline the allocator calls.
    pub(crate) fn btree_bitmap_marked_sectors(&self, start: u64, sectors: u32, with_gc: bool) -> bool {
        let shift = self.mi.get().btree_bitmap_shift;
        let end = start + u64::from(sectors);
        if end > 64u64 << shift {
            return false;
        }

        let gc = if with_gc { self.btree_allocated_bitmap_gc.get() } else { !0 };
        let want = bitmap_bits(start, end, shift);
        self.mi.get().btree_allocated_bitmap & gc & want == want
    }

    /// Whether any of [@start, @start + @sectors) is marked in its member's
    /// bitmap.
    pub(crate) fn btree_bitmap_marked_sectors_any(&self, start: u64, sectors: u32) -> bool {
        let shift = self.mi.get().btree_bitmap_shift;
        let bitmap_end = 64u64 << shift;
        if start >= bitmap_end {
            return false;
        }

        /* a bucket can run past the bitmap's end, and its bit 64 past a u64 */
        let end = (start + u64::from(sectors)).min(bitmap_end);
        self.mi.get().btree_allocated_bitmap & bitmap_bits(start, end, shift) != 0
    }

    /// ca->btree_allocated_bitmap_gc made @v: written under sb_lock, @_sb,
    /// and read without it, as ca->mi.
    fn set_btree_bitmap_gc(&self, _sb: &SbLockGuard<'_>, v: u64) {
        self.btree_allocated_bitmap_gc.set(v)
    }

    /// [@start, @start + @sectors) marked in its member's bitmap, and in gc's
    /// - the bitmap coarsened first if it doesn't reach that far. Whether the
    /// member changed.
    fn btree_bitmap_mark_sectors(&self, sb: &mut SbLockGuard<'_>, start: u64, sectors: u32) -> bool {
        let end = start + u64::from(sectors);
        let mut gc = self.btree_allocated_bitmap_gc.get();
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
        assert!(u32::from(shift) < BCH_MI_BTREE_BITMAP_SHIFT_MAX);
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
    pub(crate) fn btree_bitmap_marked(&self, k: BkeySC<'_>, with_gc: bool) -> bool {
        let Some(k) = k.arm::<Ptrs>() else { return true };
        let rcu = read_lock();
        let marked = k.ptrs(self).all(|p| self.dev_rcu(p.dev().into(), &rcu)
            .is_none_or(|ca| ca.btree_bitmap_marked_sectors(p.offset(), self.btree_sectors(), with_gc)));
        marked
    }

    /// @k's node marked on each device it points to. Whether a member
    /// changed.
    pub(crate) fn btree_bitmap_mark_locked(&self, sb: &mut SbLockGuard<'_>, k: BkeySC<'_>) -> bool {
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

    pub(crate) fn btree_bitmap_mark(&self, k: BkeySC<'_>) {
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
        progress.init_mask(self, c"btree_bitmap_gc", 0, !0);

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
                for level in 1..BTREE_MAX_DEPTH {
                    BtreeIter::new_level(&trans, btree, POS_MIN, level, BtreeIterFlags::PREFETCH)
                        .for_each(&trans, |t, iter, k| {
                            self.btree_bitmap_mark_if_unmarked(k);
                            progress.update(t, iter)
                        })?;
                }

                let root = self.btree_id_root(btree).expect("a root for every btree");
                let mut iter = BtreeNodeIter::new(&trans, btree, POS_MIN, 0, root.c.level.get().into(),
                                                  BtreeIterFlags::empty());
                lockrestart_do(&trans, |t| {
                    let n = iter.peek(t)?.expect("the root node");
                    self.btree_bitmap_mark_if_unmarked(n.node().key().as_sc());
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
                    let shift = ca.mi.get().btree_bitmap_shift;
                    old += u64::from(ca.mi.get().btree_allocated_bitmap.count_ones()) << shift;
                    let gc = ca.btree_allocated_bitmap_gc.get();
                    new += u64::from(gc.count_ones()) << shift;

                    sb.fs_member_mut(ca.dev_idx.into()).btree_allocated_bitmap = gc.into();
                }
            }
            self.write_super(&mut sb);
        }

        crate::bch_info!(self, "mi_btree_bitmap sectors {} -> {}",
                         HumanReadable(old << 9), HumanReadable(new << 9));
        Ok(())
    }
}

crate::recovery_pass!(btree_bitmap_gc => |fs: &Fs| fs.btree_bitmap_gc());

/// Daily, while rw: btree_bitmap_gc scheduled - ratelimited - if a
/// device's bitmap marks four times the sectors of its btree buckets.
pub(crate) fn btree_bitmap_gc_schedule(fs: &Fs) {
    if passes::want_ratelimit(fs, RecoveryPass::btree_bitmap_gc, 1000) {
        return;
    }

    let mut msg = LogMsg::with_level(fs, LogLevel::NOTICE);
    msg.set_suppressed(true); // run_explicit() unsuppresses

    let mut want_schedule = false;
    for ca in fs.members() {
        let u = ca.usage_read_fast();
        let btree_sectors = ca.bucket_to_sector(u.buckets[DataType::btree]);
        let bitmap_sectors = u64::from(ca.mi.get().btree_allocated_bitmap.count_ones()) << ca.mi.get().btree_bitmap_shift;

        if btree_sectors * 4 < bitmap_sectors {
            prt!(msg, "{} has {} btree buckets and {} marked in bitmap\n", *ca,
                 HumanReadable(btree_sectors << 9), HumanReadable(bitmap_sectors << 9));
            want_schedule = true;
        }
    }

    if want_schedule {
        // Ignored, as C ignored it
        let _ = passes::run_explicit(fs, &mut msg, RecoveryPass::btree_bitmap_gc,
                                     RunRecoveryPassFlags::ratelimit);
    }

    fs.maybe_schedule_btree_bitmap_gc.queue(Queue::system_long(), nsecs_to_jiffies(24 * 60 * 60 * NSEC_PER_SEC));
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
impl DevsList {
    /// Just @dev.
    pub fn single(dev: u8) -> Self {
        let mut l = Self::default();
        l.add(dev);
        l
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.nr as usize]
    }

    pub fn has(&self, dev: u8) -> bool {
        self.as_slice().contains(&dev)
    }

    /// Add @dev, if it isn't in it already. A list already full is a bug, as
    /// C's BUG_ON().
    pub fn add(&mut self, dev: u8) {
        if !self.has(dev) {
            assert!((self.nr as usize) < self.data.len(), "bch_devs_list full adding device {dev}");
            self.data[self.nr as usize] = dev;
            self.nr += 1;
        }
    }

    /// Remove @dev, keeping the rest in order.
    pub fn drop(&mut self, dev: u8) {
        if let Some(i) = self.as_slice().iter().position(|&d| d == dev) {
            self.data.copy_within(i + 1..self.nr as usize, i);
            self.nr -= 1;
        }
    }
}

impl Fs {
    /// c->devs[@dev], for as long as @rcu's section. None past the
    /// superblock's devices, or for a slot with no device.
    pub fn dev_rcu<'g>(&'g self, dev: u32, rcu: &'g ReadGuard) -> Option<&'g Dev> {
        self.devs.get(..self.nr_devices() as usize)?.get(dev as usize)?.read(rcu)
    }

    /// Device @dev's name - "(none)" for BCH_SB_MEMBER_INVALID, and for one
    /// there isn't, its number.
    pub fn member_name_to_text(&self, out: &mut Printbuf, dev: u32) {
        if dev == BCH_SB_MEMBER_INVALID {
            write!(out, "(none)");
            return;
        }
        let rcu = read_lock();
        match self.dev_rcu(dev, &rcu) {
            Some(ca) => write!(out, "{ca}"),
            None     => write!(out, "(invalid device {dev})"),
        }
    }

    /// The filesystem's devices, for as long as @rcu's section:
    /// for_each_member_device_rcu().
    pub fn devs_rcu<'g>(&'g self, rcu: &'g ReadGuard) -> impl Iterator<Item = &'g Dev> {
        // nr_devices read each step, as C's: a device added meanwhile is seen
        (0..).take_while(move |&dev| dev < self.nr_devices())
            .filter_map(move |dev| self.dev_rcu(dev, rcu))
    }

    /// Those of @devs - a mask's iter() - that there are:
    /// for_each_member_device_rcu() with a mask.
    pub fn devs_rcu_in<'g>(&'g self, devs: impl Iterator<Item = usize> + 'g, rcu: &'g ReadGuard)
        -> impl Iterator<Item = &'g Dev>
    {
        devs.filter_map(move |dev| self.dev_rcu(dev as u32, rcu))
    }

    /// The online devices: for_each_online_member_rcu().
    pub fn online_devs_rcu<'g>(&'g self, rcu: &'g ReadGuard) -> impl Iterator<Item = &'g Dev> {
        self.devs_rcu_in(self.devs_online.iter(), rcu)
    }

    /// The devices the allocator writes to: for_each_rw_member_rcu().
    pub fn rw_devs_rcu<'g>(&'g self, rcu: &'g ReadGuard) -> impl Iterator<Item = &'g Dev> {
        self.rw_devs_for_rcu(DataType::free, rcu)
    }

    /// The devices the allocator writes @data_type to.
    pub fn rw_devs_for_rcu<'g>(&'g self, data_type: DataType, rcu: &'g ReadGuard) -> impl Iterator<Item = &'g Dev> {
        self.devs_rcu_in(self.allocator.rw_devs[data_type].iter(), rcu)
    }

    /// dev_missing_atomic(), and the error for it.
    fn dev_missing(&self, dev: u32) -> BchError {
        self.dev_missing_atomic(dev);
        self.err(Errcode::ENOENT_dev_not_found)
    }

    /// Whether there's a device @dev.
    pub fn dev_exists(&self, dev: u32) -> bool {
        self.dev_rcu(dev, &read_lock()).is_some()
    }

    /// A ref on device @dev, if there is one.
    pub fn dev_get(&self, dev: u32) -> Option<DevRef> {
        // in c->devs under the RCU lock: removal takes it out before it kills
        // ref_, whose count can't reach zero till a grace period after
        self.dev_rcu(dev, &read_lock()).map(|ca| unsafe { RefGuard::get(&ca.ref_) })
    }

    /// @ca made the ref on device @dev, for a walk by device - kept if it's
    /// already that one's.
    pub fn dev_iterate(&self, ca: &mut Option<DevRef>, dev: u32) {
        if ca.as_ref().is_none_or(|ca| u32::from(ca.dev_idx) != dev) {
            *ca = self.dev_get(dev);
        }
    }

    /// The first device at index @dev or after, with a ref.
    pub fn next_dev(&self, dev: u32) -> Option<DevRef> {
        (dev..self.nr_devices()).find_map(|i| self.dev_get(i))
    }

    /// dev_get(), a missing device reported.
    pub fn dev_tryget(&self, dev: u32) -> Result<DevRef, BchError> {
        self.dev_get(dev).ok_or_else(|| self.dev_missing(dev))
    }

    /// dev_get() for a pointer of @k's. A missing device isn't an error: it's
    /// reported against @k and check_allocations scheduled to repair it
    /// (unless it's BCH_SB_MEMBER_INVALID, which names none), and we carry on
    /// without it. The error is scheduling's, e.g. restart_recovery.
    pub fn dev_bkey_tryget(&self, k: BkeySC<'_>, dev: u32) -> Result<Option<DevRef>, BchError> {
        let ca = self.dev_get(dev);
        if ca.is_none() && dev != BCH_SB_MEMBER_INVALID {
            self.dev_missing_bkey(k, dev)?;
        }
        Ok(ca)
    }

    /// Whether @bucket is one of its device's.
    pub fn dev_bucket_exists(&self, bucket: Bpos) -> bool {
        self.dev_rcu(bucket.inode as u32, &read_lock()).is_some_and(|ca| ca.bucket_valid(bucket.offset))
    }

    /// A ref on @bucket's device, if it's a device with that bucket:
    pub fn dev_bucket_tryget_noerror(&self, bucket: Bpos) -> Option<DevRef> {
        self.dev_get(bucket.inode as u32).filter(|ca| ca.bucket_valid(bucket.offset))
    }

    /// dev_bucket_tryget_noerror(), a missing device or bucket reported:
    pub fn dev_bucket_tryget(&self, bucket: Bpos) -> Result<DevRef, BchError> {
        let ca = self.dev_tryget(bucket.inode as u32)?;
        if !ca.bucket_valid(bucket.offset) {
            ca.bucket_missing(bucket.offset);
            return Err(self.err(Errcode::ENOENT_dev_bucket_not_found));
        }
        Ok(ca)
    }

    /// An io ref on device @dev, for reading or writing as R is - for writing
    /// only if it's rw.
    pub fn dev_get_ioref<R: IoRefIdx>(&self, dev: u32) -> Option<DevIoRef<R>> {
        let rcu = read_lock();
        let ca = self.io_ref_tryget::<R>(self.dev_rcu(dev, &rcu)?)?;
        // then_some(), eager: a write ref on a device that isn't rw is put
        (R::RW == READ || ca.mi.get().state == MemberState::rw).then_some(ca)
    }

    /// An io ref on @ca, one of ours found under RCU: the ref keeps the device
    /// past the section, and the guard hands it out only while it's held.
    pub(crate) fn io_ref_tryget<R: IoRefIdx>(&self, ca: &Dev) -> Option<DevIoRef<R>> {
        R::count(ca).tryget()
    }

    /// What @f makes of each device it doesn't skip: each step its own RCU
    /// section, resuming after the device the last one stopped at, as
    /// next_dev().
    fn walk<'f, T>(&'f self, mut f: impl FnMut(&Dev) -> Option<T> + 'f) -> impl Iterator<Item = T> + 'f {
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
        // as dev_get()'s
        self.walk(|ca| Some(unsafe { RefGuard::get(&ca.ref_) }))
    }

    /// The devices in a state @state_ok takes, an io ref on each, put when
    /// it's dropped: __for_each_online_member().
    pub fn io_members<R: IoRefIdx>(&self, state_ok: impl Fn(MemberState) -> bool + 'static)
                                   -> impl Iterator<Item = DevIoRef<R>> + '_ {
        self.walk(move |ca| {
            state_ok(ca.mi.get().state).then(|| self.io_ref_tryget::<R>(ca)).flatten()
        })
    }

    /// Every online device, an io ref for reading on each: for_each_online_member().
    pub fn online_members(&self) -> impl Iterator<Item = DevIoRef<DevReadRefKind>> + '_ {
        self.io_members::<DevReadRefKind>(|_| true)
    }

    /// The rw devices, an io ref for writing on each: for_each_rw_member().
    pub fn rw_members(&self) -> impl Iterator<Item = DevIoRef<DevWriteRefKind>> + '_ {
        self.io_members::<DevWriteRefKind>(|s| s == MemberState::rw)
    }

}

// ── Missing devices and buckets, reported ────────────────────────────────

impl Fs {
    /// A pointer in @k to device @dev, which there isn't: said to @out,
    /// counted as the fsck error it is, and check_allocations scheduled to
    /// repair it - the error is scheduling's. Nothing for
    /// BCH_SB_MEMBER_INVALID, which names no device.
    pub fn dev_missing_bkey_msg(&self, k: BkeySC<'_>, dev: u32, out: &mut Printbuf) -> Result<(), BchError> {
        if dev == BCH_SB_MEMBER_INVALID {
            return Ok(());
        }

        let removed = self.devs_removed.test(dev as usize);
        crate::prt!(out, "pointer to {} device {dev} in key\n{}\n",
                    if removed { "removed" } else { "nonexistent" }, k.to_text(self));

        count_fsck_err(self, if removed { id::ptr_to_removed_device } else { id::ptr_to_invalid_device }, out);
        passes::run_explicit(self, out, RecoveryPass::check_allocations,
                             RunRecoveryPassFlags::empty())
    }

    /// dev_missing_bkey_msg(), logged.
    pub fn dev_missing_bkey(&self, k: BkeySC<'_>, dev: u32) -> Result<(), BchError> {
        self.dev_missing_bkey_msg(k, dev, &mut LogMsg::new(self))
    }

    /// A pointer to device @dev, which there isn't, from where there's no key
    /// to show: logged, with a backtrace, as an inconsistency. Safe from
    /// atomic context - in a NORECLAIM scope, nothing it allocates can sleep.
    fn dev_missing_atomic(&self, dev: u32) {
        if dev == BCH_SB_MEMBER_INVALID {
            return;
        }

        let _noreclaim = MemallocFlags::noreclaim();
        let mut buf = Printbuf::new();

        crate::util::log::log_msg_start(&mut buf, self.name());
        write!(buf, "pointer to {} device {dev}\n",
               if self.devs_removed.test(dev as usize) { "removed" } else { "nonexistent" });
        let _ = prt_current_backtrace(&mut buf, 1);
        self.inconsistent_error(&mut buf);
        print_str(self, LogLevel::ERR, &mut buf);
    }
}

impl Dev {
    /// A pointer to bucket @bucket, which this device hasn't: an
    /// inconsistency.
    pub fn bucket_missing(&self, bucket: u64) {
        crate::fs_inconsistent!(self.fs(), "pointer to nonexistent bucket {bucket} on device {} (valid range {}-{})",
                                self, self.mi.get().first_bucket, self.mi.get().nbuckets);
    }
}

// ── References ───────────────────────────────────────────────────────────

/// A ref on a device - ca->ref, which its removal drains before it's freed:
/// it stays a member while this is held. Option<DevRef> has C's layout - a
/// nullable bch_dev *, ref_ being its first field.
pub type DevRef = RefGuard<Placed<PercpuRef, crate::at!(Dev.ref_)>>;

/// ca->ref_outer: keeps the bch_dev's memory alive and nothing else - its
/// removal can complete while this is held. For contexts that block on
/// state_lock, where a DevRef mustn't be held: recheck ca.removing under the
/// lock before touching member state.
pub type DevOuterRef = RefGuard<Placed<AtomicRef<()>, crate::at!(Dev.ref_outer)>>;

/// A ref on a device's kobject, as c->devs[] owns it: the last put frees it.
pub type DevKobjRef = RefGuard<Placed<crate::os::kobject::Kobject, crate::at!(Dev.kobj)>>;

impl DevRef {
    /// An outer ref on it: this ref holds ref_outer
    /// above zero, removal draining ref_ first.
    pub fn outer_ref(&self) -> DevOuterRef {
        unsafe { RefGuard::get(&self.ref_outer) }
    }
}

/// Which of a device's io refs: DevReadRefKind for io_ref_read,
/// DevWriteRefKind for io_ref_write.
pub trait IoRefIdx: 'static {
    const RW: usize;
    type Count: DevIoCount;

    fn count(ca: &Dev) -> &Self::Count;
}

impl IoRefIdx for DevReadRefKind {
    const RW: usize = READ;
    type Count = Placed<PercpuRef, crate::at!(Dev.io_ref_read)>;

    fn count(ca: &Dev) -> &Self::Count {
        &ca.io_ref_read
    }
}

impl IoRefIdx for DevWriteRefKind {
    const RW: usize = WRITE;
    type Count = Placed<PercpuRef, crate::at!(Dev.io_ref_write)>;

    fn count(ca: &Dev) -> &Self::Count {
        &ca.io_ref_write
    }
}

/// A count whose refs keep the device's block device open: an io ref's.
///
/// # Safety
/// While a ref on it is held, the device's block device is published and
/// open: sb_close()'s callers stop both io refs first.
pub unsafe trait DevIoCount: TryRefcount<Target = Dev> {}

unsafe impl DevIoCount for <DevReadRefKind as IoRefIdx>::Count {}
unsafe impl DevIoCount for <DevWriteRefKind as IoRefIdx>::Count {}

impl<C: DevIoCount> RefGuard<C> {
    /// Its block device: open, as the ref is held.
    pub fn bdev(&self) -> &crate::os::blkdev::BlockDevice {
        unsafe { self.disk_sb.bdev.get().as_ref() }.expect("an io ref on a device with no block device")
    }

    /// Its block device's name, model and serial, and whether it's
    /// rotational, recorded in its member.
    pub fn mi_field_upgrades(&self) {
        let id = identity(self.bdev());

        let fs = self.fs();
        let mut sb = fs.sb_lock();
        if mi_field_upgrades(&mut sb, self.dev_idx.into(), &id) {
            fs.write_super(&mut sb);
        }
    }
}

/// @bdev's name, model and serial, and whether it's rotational.
pub(crate) fn identity(bdev: &crate::os::blkdev::BlockDevice) -> DevIdentity {
    let mut id = ::pin_init::zeroed::<DevIdentity>();
    crate::init::dev::read_identity(bdev, &mut id);
    id.rotational = crate::os::blkdev::rotational(bdev);
    id
}

/// What a guard of @N covers on each of its filesystem's devices - each
/// device's LockedBy at path @F - had by &mut, several devices' at once: a
/// split of the guard, by device. take() hands one out for as long as the
/// split lives, get_mut() for as long as it's borrowed; either, of a device
/// taken, panics.
pub(crate) struct DevPieces<'g, N, F> {
    fs:    &'g Fs,
    taken: DevsMask,
    _p:    PhantomData<(N, F)>,
}

impl<'g, N: 'static, F> DevPieces<'g, N, F>
where
    F: crate::util::place::Path<Dev>,
    F::To: crate::locking::AsLockedBy<L = N> + 'static,
{
    /// # Safety
    /// A guard of N, held alone, for 'g - which only this hands out F of
    /// @fs's devices, meanwhile.
    pub(crate) unsafe fn new(fs: &'g Fs) -> Self {
        DevPieces { fs, taken: Default::default(), _p: PhantomData }
    }

    fn check(&self, ca: &Dev) {
        assert!(core::ptr::eq(ca.fs(), self.fs), "a split of another filesystem's device {}", ca.dev_idx);
        assert!(!self.taken.test(ca.dev_idx.into()), "device {} taken from a split twice", ca.dev_idx);
    }

    /// @ca's, for as long as the split lives.
    pub(crate) fn take(&mut self, ca: &'g Dev) -> &'g mut <F::To as crate::locking::AsLockedBy>::G {
        self.check(ca);
        self.taken.set(ca.dev_idx.into());
        unsafe { crate::locking::AsLockedBy::locked_by(F::get(ca)).protected_mut() }
    }

    /// @ca's, for as long as this is borrowed.
    pub(crate) fn get_mut<'d>(&'d mut self, ca: &'d Dev) -> &'d mut <F::To as crate::locking::AsLockedBy>::G {
        self.check(ca);
        unsafe { crate::locking::AsLockedBy::locked_by(F::get(ca)).protected_mut() }
    }
}


/// A device's name, C's char array: published, as attaching it renames it,
/// so a reader has a copy - torn, if it raced the rename: each byte some
/// name's, and the last a NUL, as every name's is.
#[repr(transparent)]
#[derive(Clone, Copy, ::pin_init::Zeroable, cstruct_macros::Plain)]
pub struct DevName([c_char; c::BDEVNAME_SIZE as usize]);

impl TearSafe for DevName {}

impl DevName {
    /// @s, up to a NUL in it, truncated to leave room for its own.
    pub fn new(s: &[u8]) -> Self {
        let mut n = Self([0; c::BDEVNAME_SIZE as usize]);
        crate::util::ffi::strscpy(&mut n.0, s);
        n
    }

    /// Up to its NUL, or empty if it has none.
    pub fn as_cstr(&self) -> &CStr {
        crate::util::ffi::cstr_in(&self.0)
    }

    pub fn to_str(&self) -> Result<&str, core::str::Utf8Error> {
        self.as_cstr().to_str()
    }

    pub fn to_bytes(&self) -> &[u8] {
        self.as_cstr().to_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.0[0] == 0
    }
}

impl core::fmt::Display for DevName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.to_str().unwrap_or("?"))
    }
}

/// An io ref on a device - for reading or writing, as R is: its block
/// device is open while it's held.
pub type DevIoRef<R> = RefGuard<<R as IoRefIdx>::Count>;

/// A block IO's device, by its io ref: a read's, or a write's.
pub enum BlockDev {
    Read(DevIoRef<DevReadRefKind>),
    Write(DevIoRef<DevWriteRefKind>),
}

impl From<DevIoRef<DevReadRefKind>> for BlockDev {
    fn from(ca: DevIoRef<DevReadRefKind>) -> Self {
        BlockDev::Read(ca)
    }
}

impl From<DevIoRef<DevWriteRefKind>> for BlockDev {
    fn from(ca: DevIoRef<DevWriteRefKind>) -> Self {
        BlockDev::Write(ca)
    }
}

impl BlockDev {
    pub fn bdev(&self) -> &crate::os::blkdev::BlockDevice {
        match self {
            BlockDev::Read(ca)  => ca.bdev(),
            BlockDev::Write(ca) => ca.bdev(),
        }
    }
}

impl core::ops::Deref for BlockDev {
    type Target = Dev;

    fn deref(&self) -> &Dev {
        match self {
            BlockDev::Read(ca)  => ca,
            BlockDev::Write(ca) => ca,
        }
    }
}

impl Dev {
    /// Its io ref count for @rw, READ or WRITE.
    pub fn io_ref(&self, rw: usize) -> &PercpuRef {
        match rw {
            READ  => &self.io_ref_read,
            WRITE => &self.io_ref_write,
            _     => panic!("io ref of a device for {rw}, not READ or WRITE"),
        }
    }
}

// SAFETY: a device is every thread's that does IO to it, as its filesystem
// is: its state behind its own locks, atomics and RCU - so a &bch_dev goes to
// any thread too, and an io ref, a percpu ref, is put from anywhere. What
// Rust reads through it is as synchronized as the C it reads.
unsafe impl Sync for Dev {}

impl Fs {
    /// Whether device @dev has its block device open.
    pub fn dev_idx_is_online(&self, dev: u32) -> bool {
        self.devs_online.test(dev as usize)
    }

    pub fn dev_rotational(&self, dev: u32) -> bool {
        dev != BCH_SB_MEMBER_INVALID && self.devs_rotational.test(dev as usize)
    }

    /// Whether @dev is missing, or being evacuated.
    pub fn dev_bad_or_evacuating(&self, dev: u32, rcu: &ReadGuard) -> bool {
        self.dev_rcu(dev, rcu).is_none_or(|ca| {
            ca.mi.get().state == MemberState::evacuating
        })
    }
}

impl Dev {
    /// Its filesystem.
    pub fn fs(&self) -> &Fs {
        &self.fs
    }

    /// ca->mi made @mi: see "In memory" above.
    fn set_mi(&self, _sb: &SbLockGuard<'_>, mi: MemberCpu) {
        self.mi.set(mi)
    }

    /// Its IO errors, by kind: since the filesystem was made, and since they
    /// were last reset.
    pub fn io_errors_to_text(&self, out: &mut Printbuf) {
        let m = sb_member(self.fs().sb_lock().sb(), self.dev_idx.into()).unwrap_or_else(::pin_init::zeroed);
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
        prt!(out, "IO errors since {} ago\n", Nsecs(since.wrapping_mul(NSEC_PER_SEC)));
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

    /// @sectors of IO of @data_type, reading or writing as @rw, counted in
    /// io_done: this_cpu_add(ca->io_done->sectors[rw][data_type], sectors).
    pub fn io_done_add(&self, rw: usize, data_type: DataType, sectors: u64) {
        crate::percpu_field!(self.io_done.get(), sectors[rw][data_type.0 as usize]).add(sectors);
    }

    /// An IO of type @ty, submitted at @submit_time, completed: its latency -
    /// unless it's a checksum check, which isn't IO - and a success, which
    /// ends a run of write errors, or a failure, counted against the device.
    /// From any context, an endio's included.
    pub fn account_io_completion(&self, ty: MemberErrorType, submit_time: u64, success: bool) {
        if ty != MemberErrorType::checksum {
            // @ty is read or write: the rw it takes
            self.io_latency_acct(submit_time, ty.0 as usize);
        }
        self.account_io_result(ty, success);
    }

    /// latency_acct(), unless the build doesn't account latency.
    pub fn io_latency_acct(&self, submit_time: u64, rw: usize) {
        #[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
        self.latency_acct(submit_time, rw);
        #[cfg(CONFIG_BCACHEFS_NO_LATENCY_ACCT)]
        let _ = (submit_time, rw);
    }

    /// account_io_completion() but for the latency: what can wait till the
    /// IO's waiter has it.
    pub fn account_io_result(&self, ty: MemberErrorType, success: bool) {
        use crate::sb::members_format::MemberErrorType as T;

        if success {
            if ty == T::write && self.write_errors_start.load(Ordering::Relaxed) != 0 {
                self.write_errors_start.store(0, Ordering::Relaxed);
            }
        } else {
            self.io_error(ty);
        }
    }

    /// Whether it's being removed - set by dev_remove() before the data
    /// drop, cleared if the removal fails: read once, as C's READ_ONCE().
    pub fn removing(&self) -> bool {
        self.removing.get()
    }

    /// A write completed that the device may still have in its volatile
    /// cache, for the journal's next preflush.
    pub fn write_unflushed(&self) {
        let f = DevFlag::unflushed_writes;
        if !self.flags.get(f) {
            self.flags.set(f, true);
        }
    }

    /// The latest IO's latency, for reads (READ) or writes (WRITE).
    pub fn cur_latency(&self, rw: usize) -> i64 {
        self.cur_latency[rw].load(Ordering::Relaxed)
    }

    /// The data types with data on it, by the superblock's replicas.
    pub fn has_data(&self) -> DataTypes {
        self.fs().sb_lock().sb().dev_has_data(self.dev_idx.into())
    }

    /// Whether @b is one of its buckets: bucket_valid().
    pub fn bucket_valid(&self, b: u64) -> bool {
        b.wrapping_sub(self.mi.get().first_bucket.into()) < self.mi.get().nbuckets_minus_first
    }

    /// The block device's name - sdX and the like - as it was a moment ago.
    pub fn name(&self) -> DevName {
        self.name.get()
    }

    /// For C functions taking *mut bch_dev.
    pub fn as_ptr(&self) -> *mut Dev {
        core::ptr::from_ref(self).cast_mut()
    }

    /// Whether its block device is open for IO.
    pub fn is_online(&self) -> bool {
        !self.io_ref_read.is_zero()
    }

    /// Whether the allocator writes to it.
    pub fn is_rw(&self) -> bool {
        self.fs.allocator.rw_devs[DataType::free].test(self.dev_idx.into())
    }

    /// What a new copy written to it would be worth - nothing unless it's rw:
    pub fn rw_durability(&self) -> u32 {
        if self.is_rw() { self.mi.get().durability.into() } else { 0 }
    }
}

/// A device prints as its name, as C's messages name it.
impl ToText for Dev {
    fn to_text(&self, out: &mut Printbuf) {
        out.write_bytes(self.name().to_bytes())
    }
}
crate::display_via_to_text!(Dev);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::dev::SbHandle;
    use crate::util::vstructs::plain_bytes_mut;

    fn new_sb(nr: u8) -> SbHandle {
        let mut sb = SbHandle::default();
        sb.sb_realloc(0).unwrap();
        sb.sb_mut().nr_devices = nr;
        sb
    }

    fn member(i: u8) -> Member {
        let mut m = ::pin_init::zeroed::<Member>();
        m.uuid.b = [i + 1; 16];
        m.nbuckets = (1000 * u64::from(i) + 1).into();
        m
    }

    /// @nr members, each its first @bytes, at a stride of @bytes - as an
    /// older version wrote them.
    fn put_v2(sb: &mut SbHandle, nr: u8, bytes: usize) {
        let u64s = (MEMBERS_V2_HEAD + bytes * usize::from(nr)).div_ceil(8) as u32;
        let mi = sb.field_resize::<SbFieldMembersV2>(u64s).unwrap();
        mi.member_bytes = (bytes as u16).into();
        let mut m = members_v2_mut(mi).unwrap();
        for i in 0..nr {
            m.slot_bytes_mut(i.into()).unwrap().copy_from_slice(&plain_bytes(&member(i))[..bytes]);
        }
    }

    fn whole(sb: &mut SbHandle, i: u8) -> Member {
        *sb.member_mut(i.into()).expect("a whole member")
    }

    /// From members_v1: each member as it was, zero past the 56 bytes v1 has.
    #[test]
    fn members_v2_from_v1() {
        let mut sb = new_sb(3);
        let u64s = (MEMBERS_V1_HEAD + BCH_MEMBER_V1_BYTES * 3).div_ceil(8) as u32;
        sb.field_resize::<SbFieldMembersV1>(u64s).unwrap();
        for i in 0..3 {
            sb.put_member_v1::<SbFieldMembersV1>(MEMBERS_V1_HEAD, i.into(), &member(i));
        }

        sb.members_v2_init().unwrap();
        let mi = sb.field::<SbFieldMembersV2>().unwrap();
        assert_eq!(usize::from(mi.member_bytes.get()), size_of::<Member>());
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
            let mut want = ::pin_init::zeroed::<Member>();
            plain_bytes_mut(&mut want)[..BCH_MEMBER_V1_BYTES]
                .copy_from_slice(&plain_bytes(&member(i))[..BCH_MEMBER_V1_BYTES]);
            assert_eq!(plain_bytes(&whole(&mut sb, i)), plain_bytes(&want));
        }
    }

    /// members_v1's to_text counts its members by its own stride: C's used
    /// Member's, so printed a few and said nr_devices didn't match.
    #[test]
    fn members_v1_to_text() {
        let mut sb = new_sb(3);
        let u64s = (MEMBERS_V1_HEAD + BCH_MEMBER_V1_BYTES * 3).div_ceil(8) as u32;
        sb.field_resize::<SbFieldMembersV1>(u64s).unwrap();
        for i in 0..3 {
            sb.put_member_v1::<SbFieldMembersV1>(MEMBERS_V1_HEAD, i.into(), &member(i));
        }

        let mut out = Printbuf::new();
        members_to_text(&mut out, sb.sb(), sb.field::<SbFieldMembersV1>().unwrap().members());
        assert!(!out.as_str().contains("mismatch"));
        assert_eq!(out.as_str().matches("Device:").count(), 3);
    }

    /// The mirror is each member's first 56 bytes - and, past extent_flags,
    /// gone: versions that read it can't read the superblock.
    #[test]
    fn members_v1_mirror() {
        let mut sb = new_sb(2);
        put_v2(&mut sb, 2, size_of::<Member>());
        sb.members_cpy_v2_v1().unwrap();
        for i in 0..2 {
            let m = members_v1(sb.sb()).unwrap().get(i.into()).unwrap();
            assert_eq!(plain_bytes(&m)[..BCH_MEMBER_V1_BYTES], plain_bytes(&member(i))[..BCH_MEMBER_V1_BYTES]);
        }

        sb.sb_mut().set_version_incompat(MetadataVersion(MetadataVersion::extent_flags.0 + 1));
        sb.members_cpy_v2_v1().unwrap();
        assert!(sb.field::<SbFieldMembersV1>().is_none());
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
