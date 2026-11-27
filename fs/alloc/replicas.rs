// SPDX-License-Identifier: GPL-2.0

//! Replicas: which sets of devices hold data of which type, so the filesystem
//! knows what it can still read with devices missing.
//!
//! An entry is a data type, how many of its devices are required to read it
//! - nr_required, 0 for erasure coded - and the devices, sorted:
//! ReplicasEntryV1, a #[counted] DST. The superblock lists them in the
//! replicas section - or replicas_v0, whose entries have no nr_required,
//! when every entry's would be 1 - back to back, each as long as its device
//! list, the section zero padded after the last.
//!
//! An entry being built - from a key, a device list - is written into room
//! for a key's worth of devices, BCH_BKEY_PTRS_MAX: C's bch_replicas_padded,
//! or a disk_accounting_pos, whose replicas entry is that size exactly.

#![allow(non_camel_case_types)]

use crate::alloc::accounting::{CounterSet, DiskAccountingPos, ReplicasCounters};
use crate::alloc::accounting_mem::{self, MarkWrite};
use crate::btree::bkey::{BkeySC, BkeyV, ValidateFlags};
use crate::data::{ec::format::Stripe, ptrs::{BkeyVPtrs, Ptrs}};
use crate::alloc::replicas_format::{ReplicasEntryV0, ReplicasEntryV1,
    SbFieldReplicas, SbFieldReplicasV0};
use crate::bcachefs_ioctl::ForceIf;
use crate::init::{dev::{DevsList, DevsMask}, passes::RecoveryPass};
use crate::sb::members_format::BCH_SB_MEMBER_INVALID;
use crate::errcode::{Errcode, BchError};
use crate::format::{DataType, DataTypes, MetadataVersion, BCH_BKEY_PTRS_MAX, Sb, SbFieldType};
use crate::fs::{Fs, SbLockGuard, FsFlag};
use crate::sb::io::{entries_end, SbFieldOps, SbSection, SbState, SbResize};
use crate::sb::members::member_exists;
use crate::os::kernel::{cold_path, likely};
use crate::util::rcu::read_lock;
use crate::util::alloc::{AllocError, Vec};
use crate::data_structures::{eytzinger::Eytzinger0, dst_room::DstRoom, strided::StridedBuf};
use crate::util::vstructs::{bytes_of_mut, cast_plain_mut, Head, State, Unsized, Unvalidated,
                            Validated};
use crate::util::{Printbuf, ToText};
use core::fmt;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicI32, Ordering};
use cstruct_macros::bch_enum;

#[bch_enum(closed)]
#[repr(u32)]
pub enum WriteCheck {
    /* Starting: is there anywhere to write each data type at all? */
    BCH_WRITE_CHECK_start,
    /*
     * A device leaving the rw set: also refuse, unless the matching force
     * flag is set, if that takes a data type below its configured replicas
     */
    BCH_WRITE_CHECK_dev_leaving_rw,
}

type Entry = ReplicasEntryV1<Validated>;

/// Where an entry's devices start: after data_type, nr_devs, nr_required.
const DEVS: usize = offset_of!(Head<ReplicasEntryV1>, devs);

/// An entry in room for a key's worth of devices.
pub type ReplicasPadded = DstRoom<Entry, [u8; ENTRY_BYTES]>;

/// Where a replicas section's entries start - either section's.
const SECTION_HEAD: usize = size_of::<Head<SbFieldReplicas>>();
const _: () = assert!(SECTION_HEAD == size_of::<Head<SbFieldReplicasV0>>());

// ── Entries ──────────────────────────────────────────────────────────────

/// @items, a space between each.
fn prt_spaced<T>(out: &mut Printbuf, items: impl IntoIterator<Item = T>, mut f: impl FnMut(&mut Printbuf, T)) {
    for (i, t) in items.into_iter().enumerate() {
        if i != 0 {
            write!(out, " ");
        }
        f(out, t);
    }
}

/// An entry as the superblock lists it, either section's: replicas_v0's have
/// no nr_required - it's 1.
#[derive(Clone, Copy)]
pub struct SbEntry<'a> {
    pub data_type:   DataType,
    pub nr_required: Option<u8>,
    pub devs:        &'a [u8],
}

impl<'a, S: State> From<&'a ReplicasEntryV1<S>> for SbEntry<'a> {
    fn from(e: &'a ReplicasEntryV1<S>) -> Self {
        SbEntry { data_type: e.data_type.get(), nr_required: Some(e.nr_required), devs: &e.devs }
    }
}

impl<'a, S: State> From<&'a ReplicasEntryV0<S>> for SbEntry<'a> {
    fn from(e: &'a ReplicasEntryV0<S>) -> Self {
        SbEntry { data_type: e.data_type.get(), nr_required: None, devs: &e.devs }
    }
}

impl SbEntry<'_> {
    pub(crate) fn to_text(&self, out: &mut Printbuf) {
        self.data_type.to_text(out);
        match self.nr_required {
            Some(r) => write!(out, ": {r}/{} [", self.devs.len()),
            None    => write!(out, ": {} [", self.devs.len()),
        };
        prt_spaced(out, self.devs, |out, d| write!(out, "{d}"));
        write!(out, "]");
    }

    /// Devices, nr_required fewer than them - or 1, a single device's - and
    /// none that @dev_ok says isn't one.
    pub(crate) fn validate(&self, dev_ok: impl Fn(u8) -> bool, err: &mut Printbuf) -> Result<(), BchError> {
        let invalid = |err: &mut Printbuf, msg: fmt::Arguments<'_>| {
            write!(err, "{msg} in entry ");
            self.to_text(err);
            Err(Errcode::invalid_replicas_entry.into())
        };

        if self.devs.is_empty() {
            return invalid(err, format_args!("no devices"));
        }
        if self.nr_required.is_some_and(|r| r > 1 && usize::from(r) >= self.devs.len()) {
            return invalid(err, format_args!("bad nr_required"));
        }
        for &d in self.devs {
            if u32::from(d) != BCH_SB_MEMBER_INVALID && !dev_ok(d) {
                return invalid(err, format_args!("invalid device {d}"));
            }
        }
        Ok(())
    }
}

impl ToText for Entry {
    fn to_text(&self, out: &mut Printbuf) {
        SbEntry::from(self).to_text(out)
    }
}
crate::display_via_to_text!(Entry);

impl Entry {
    pub(crate) fn validate(&self, dev_ok: impl Fn(u8) -> bool, err: &mut Printbuf) -> Result<(), BchError> {
        SbEntry::from(self).validate(dev_ok, err)
    }
}

/// An entry with room for a key's worth of devices, in bytes:
/// bch_replicas_padded's.
const ENTRY_BYTES: usize = DEVS + BCH_BKEY_PTRS_MAX as usize;

/// An entry being built in its room: its head, then the devices past it -
/// what has the room sets the count.
struct EntryBuf<'a>(&'a mut DstRoom<Entry, [u8]>);

impl EntryBuf<'_> {
    fn head(&mut self) -> &mut Head<Entry> {
        cast_plain_mut(self.0.elems_mut()).expect("a replicas entry's room holds its head")
    }

    fn start(&mut self, data_type: DataType, nr_required: u8) {
        let h = self.head();
        h.data_type.set(data_type);
        h.nr_devs     = 0;
        h.nr_required = nr_required;
    }

    fn set_nr_required(&mut self, nr_required: u8) {
        self.head().nr_required = nr_required;
    }

    /// replicas_entry_add_dev(): past the room, a bug - a key has no more
    /// pointers than that.
    fn add_dev(&mut self, dev: u8) {
        let nr = usize::from(self.head().nr_devs);
        self.0.elems_mut()[DEVS + nr] = dev;
        self.head().nr_devs += 1;
    }

    fn sort(&mut self) {
        self.0.devs.sort_unstable();
    }

    /// The entry for @data_type data on @devs, @nr_required of them needed.
    fn fill(&mut self, data_type: DataType, nr_required: u8, devs: &[u8]) {
        self.start(data_type, nr_required);
        devs.iter().for_each(|&d| self.add_dev(d));
        self.sort();
    }
}

/// The entry for @k's data: its pointers' devices, the cached ones aside -
/// a pointer into a stripe makes it erasure coded, nr_required 0 - or a
/// stripe's blocks'. A key with no data leaves the data type as it was, and
/// no devices.
fn bkey_to_replicas(fs: &Fs, e: &mut EntryBuf<'_>, k: BkeySC<'_>) {
    type D = DataType;

    e.head().nr_devs = 0;

    if let Some(s) = k.arm::<Stripe>() {
        e.start(D::parity, s.v().nr_data() as u8);
        for ptr in s.v().block_ptrs() {
            e.add_dev(ptr.dev());
        }
    } else if let Some(kp) = k.arm::<Ptrs>() {
        e.start(if k.k.is_btree_ptr() { D::btree } else { D::user }, 1);
        for p in kp.ptrs_decoded(fs) {
            if p.ptr.cached() != 0 {
                continue;
            }
            if p.p.has_ec {
                e.set_nr_required(0);
            } else {
                e.add_dev(p.ptr.dev());
            }
        }
    }
    e.sort();
}

/// The replicas accounting position for @data_type data on @devs, of which
/// @nr_required are needed to read it.
pub fn replicas_acct(data_type: DataType, nr_required: u8, devs: &[u8]) -> DiskAccountingPos {
    try_replicas_acct(data_type, nr_required, devs).expect("a key's devices fit an accounting position")
}

/// replicas_acct(), or None if @devs don't fit in an accounting position.
pub(crate) fn try_replicas_acct(data_type: DataType, nr_required: u8, devs: &[u8])
    -> Option<DiskAccountingPos>
{
    let mut e = Head::<ReplicasEntryV1>::default();
    e.data_type.set(data_type);
    e.nr_devs     = devs.len() as u8;
    e.nr_required = nr_required;
    DiskAccountingPos::from_arm_tailed(e, devs).ok()
}

/// replicas_acct() of @e, for @data_type data, whatever type it says.
pub(crate) fn entry_acct(e: &Entry, data_type: DataType) -> DiskAccountingPos {
    replicas_acct(data_type, e.nr_required, &e.devs)
}

/// @k's entry, as bkey_to_replicas() makes it: what gc keeps of a stripe's.
pub(crate) fn bkey_replicas_padded(fs: &Fs, k: BkeySC<'_>) -> ReplicasPadded {
    let mut r = ReplicasPadded::default();
    bkey_to_replicas(fs, &mut EntryBuf(&mut r), k);
    r
}

/// The entry for @data_type data on @devs, in room of its own.
pub(crate) struct DevlistEntry(ReplicasPadded);

impl DevlistEntry {
    pub(crate) fn new(data_type: DataType, devs: DevsList) -> Self {
        assert!(data_type.0 != 0 &&
                data_type != DataType::sb &&
                data_type.0 < DataType::NR,
                "replicas entry of data type {}", data_type.0);

        let mut r = ReplicasPadded::default();
        EntryBuf(&mut r).fill(data_type, 1, devs.as_slice());
        DevlistEntry(r)
    }
}

impl PartialEq for DevlistEntry {
    /* Zeroed, then filled in: bytewise is bch2_replicas_entry_eq() */
    fn eq(&self, other: &Self) -> bool {
        self.0.elems() == other.0.elems()
    }
}

impl core::ops::Deref for DevlistEntry {
    type Target = Entry;

    fn deref(&self) -> &Entry {
        &self.0
    }
}

/// replicas_acct() of @k's entry, as bkey_to_replicas() makes it, for
/// @data_type data.
pub(crate) fn bkey_replicas_acct(fs: &Fs, k: BkeySC<'_>, data_type: DataType) -> DiskAccountingPos {
    let mut r = ReplicasPadded::default();
    bkey_to_replicas(fs, &mut EntryBuf(&mut r), k);
    entry_acct(&r, data_type)
}

// ── The superblock's entries ─────────────────────────────────────────────

/// A section's entries, walked: up to the zeroes after the last.
fn up_to_zeroes<'a, E: ?Sized + 'a>(entries: impl Iterator<Item = &'a E>) -> impl Iterator<Item = SbEntry<'a>>
where
    &'a E: Into<SbEntry<'a>>,
{
    entries.map(Into::into).take_while(|e: &SbEntry<'_>| e.data_type != DataType::free)
}

/// The entries of a replicas section as read: as far as they go - the zeroes
/// after the last, or one that doesn't fit.
fn v1_entries(f: &SbFieldReplicas<Unvalidated>) -> impl Iterator<Item = SbEntry<'_>> {
    up_to_zeroes(f.entries().map_while(Result::ok))
}

/// v1_entries(), of replicas_v0.
fn v0_entries(f: &SbFieldReplicasV0<Unvalidated>) -> impl Iterator<Item = SbEntry<'_>> {
    up_to_zeroes(f.entries().map_while(Result::ok))
}

/// Replicas' entries, if the superblock has the section, else replicas_v0's.
fn v1_or_v0<'a, V1, V0>(v1: Option<V1>, v0: impl FnOnce() -> Option<V0>) -> impl Iterator<Item = SbEntry<'a>>
where
    V1: Iterator<Item = SbEntry<'a>>,
    V0: Iterator<Item = SbEntry<'a>>,
{
    let v0 = v1.is_none().then(v0).flatten();
    v1.into_iter().flatten().chain(v0.into_iter().flatten())
}

/// A superblock's replicas entries, as its state walks them: Validated,
/// trusting the section, as C does; Unvalidated, as read.
pub trait SbReplicas: State + Sized {
    fn sb_entries(sb: &Sb<Self>) -> impl Iterator<Item = SbEntry<'_>>;
}

impl SbReplicas for Validated {
    fn sb_entries(sb: &Sb) -> impl Iterator<Item = SbEntry<'_>> {
        v1_or_v0(sb.field::<SbFieldReplicas>().map(|f| up_to_zeroes(f.entries())),
                 || sb.field::<SbFieldReplicasV0>().map(|f| up_to_zeroes(f.entries())))
    }
}

impl SbReplicas for Unvalidated {
    fn sb_entries(sb: &Sb<Unvalidated>) -> impl Iterator<Item = SbEntry<'_>> {
        v1_or_v0(Self::field::<SbFieldReplicas>(sb).map(v1_entries),
                 || Self::field::<SbFieldReplicasV0>(sb).map(v0_entries))
    }
}

impl<S: SbState> Sb<S> {
    /// Whether it lists journal replicas.
    pub fn has_journal(&self) -> bool {
        S::sb_entries(self).any(|e| u32::from(e.data_type) == DataType::journal.0)
    }

    /// The data types with data on @dev.
    pub fn dev_has_data(&self, dev: u32) -> DataTypes {
        S::sb_entries(self)
            .filter(|e| e.data_type.0 < 32 && e.devs.iter().any(|&d| u32::from(d) == dev))
            .fold(DataTypes::empty(), |m, e| m | e.data_type.into())
    }
}

// ── Can we read, can we write ────────────────────────────────────────────

/// Whether data that @e lists can be read with @devs: none of its devices
/// missing past what @flags allows - degraded, or lost. If
/// not, why in @err.
pub(crate) fn can_read_with_devs(e: &Entry, devs: &DevsMask, flags: ForceIf, err: Option<&mut Printbuf>)
    -> bool
{
    type D = DataType;

    if e.data_type == D::cached {
        return true;
    }

    let invalid = e.devs.iter().filter(|&&d| u32::from(d) == BCH_SB_MEMBER_INVALID).count();
    let online = e.devs.iter()
        .filter(|&&d| u32::from(d) != BCH_SB_MEMBER_INVALID && devs.test(d.into()))
        .count();

    if invalid == e.devs.len() {
        return true;
    }

    let metadata = e.data_type.get() < D::user;
    let mut dflags = ForceIf::empty();
    if online < usize::from(e.nr_required) {
        dflags |= if metadata { ForceIf::METADATA_LOST } else { ForceIf::DATA_LOST };
    }
    if online + invalid < e.devs.len() {
        dflags |= if metadata { ForceIf::METADATA_DEGRADED } else { ForceIf::DATA_DEGRADED };
    }

    if !flags.contains(dflags) {
        if let Some(err) = err {
            write!(err, "insufficient devices online ({online}) for replicas entry ");
            e.to_text(err);
            err.newline();
        }
        return false;
    }
    true
}

/// Whether the filesystem can write with @devs - at start, the rw set; for a
/// device leaving rw, the rw set without it: somewhere for journal, btree and
/// user data at all, and leaving rw, no fewer replicas than configured of
/// each - than there are now, if that's fewer - unless @flags allows going
/// degraded. If not, why in @err.
pub(crate) fn can_write_with_devs(fs: &Fs, devs: &DevsMask, check: WriteCheck,
                       flags: ForceIf, err: &mut Printbuf) -> bool {
    type D = DataType;

    // have: what the rw set has now; online: what @devs would have
    let mut have = [0u32; DataType::NR as usize];
    let mut online = [0u32; DataType::NR as usize];

    let rcu = read_lock();
    for ca in fs.devs_rcu_in(fs.allocator.rw_devs[0].iter(), &rcu) {
        let durability = u32::from(ca.mi.get().durability);
        if durability == 0 {
            continue;
        }

        let is_online = devs.test(ca.dev_idx.into());
        for (i, (h, o)) in have.iter_mut().zip(&mut online).enumerate() {
            if ca.mi.get().data_allowed.contains(DataType(i as u32).into()) {
                *h += durability;
                *o += durability * is_online as u32;
            }
        }
    }
    drop(rcu);

    let (j, b, u) = (D::journal.0 as usize, D::btree.0 as usize, D::user.0 as usize);

    for (t, name) in [(j, "journal"), (b, "btree"), (u, "user data")] {
        if online[t] == 0 {
            write!(err, "No rw {name} devices online\n");
            return false;
        }
    }

    if check == WriteCheck::BCH_WRITE_CHECK_start {
        return true;
    }

    let opts = fs.opts();
    let journal_replicas = match opts.journal_replicas.get() {
        0 => opts.metadata_replicas.get(),
        r => r,
    };
    let want = |t: usize, force: ForceIf, name: &str, replicas: u8, err: &mut Printbuf| {
        if !flags.intersects(force) && online[t] < have[t] && online[t] < u32::from(replicas) {
            write!(err, "Insufficient rw {name} devices ({} < {replicas}) online\n", online[t]);
            return false;
        }
        true
    };

    want(j, ForceIf::METADATA_DEGRADED, "journal", journal_replicas, err) &&
    want(b, ForceIf::METADATA_DEGRADED, "btree", opts.metadata_replicas.get(), err) &&
    want(u, ForceIf::DATA_DEGRADED, "user data", opts.data_replicas.get(), err)
}

// ── In memory: c->replicas ───────────────────────────────────────────────
//
// The superblock's entries, for lookup: in eytzinger order, each zero padded
// to the size of the biggest and compared as bytes. A search compares its own
// bytes, nr_devs among them, so it never matches a longer entry.
//
// The table has a lock of its own, readers many and writers rare. The order
// is sb_lock, then mark_lock - where accounting needs one of its entries and
// the replicas entry to change together: its insert's check, kill,
// gc_accounted - then this. write_super() takes it for read, in
// can_read_fs_with_devs(): it's never held across one.
//
// Journal entries are refcounted - a ref for each journal write holding one,
// until reclaim puts it - and the last put kills the entry. The refs are
// atomics beside the table, taken and put under the read lock.

pub struct ReplicasCpu {
    entries: StridedBuf<Entry>,
    refs:    Vec<AtomicI32>,
}

impl ReplicasCpu {
    pub const fn new() -> Self {
        ReplicasCpu { entries: StridedBuf::new(), refs: Vec::new() }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn entry(&self, i: usize) -> &Entry {
        self.entries.slot(i)
    }

    fn entries(&self) -> impl Iterator<Item = &Entry> {
        (0..self.len()).map(|i| self.entry(i))
    }

    fn with_refs(&self) -> impl Iterator<Item = (&Entry, &AtomicI32)> {
        self.entries().zip(self.refs.iter())
    }

    /// @nr slots of @stride bytes, zeroed, their refs 0.
    fn zeroed(nr: usize, stride: usize) -> Result<Self, AllocError> {
        let mut refs = Vec::with_capacity(nr)?;
        for _ in 0..nr {
            refs.push(AtomicI32::new(0))?;
        }
        Ok(ReplicasCpu { entries: StridedBuf::zeroed(nr, stride)?, refs })
    }

    /// The entries @walk yields that @keep keeps, each one's devices sorted -
    /// the table not.
    fn from_entries<'a, I: Iterator<Item = SbEntry<'a>>>(walk: impl Fn() -> I, keep: impl Fn(DataType) -> bool)
        -> Result<Self, AllocError>
    {
        let kept = || walk().filter(|e| keep(e.data_type));
        let mut r = Self::zeroed(kept().count(), kept().map(|e| DEVS + e.devs.len()).max().unwrap_or(0))?;

        for (i, e) in kept().enumerate() {
            EntryBuf(r.entries.slot_mut(i)).fill(e.data_type, e.nr_required.unwrap_or(1), e.devs);
        }
        Ok(r)
    }

    fn sort(&mut self) {
        Eytzinger0::<usize>::new(self.len()).sort(self,
            |r, i, j| r.entries.slot(i).elems().cmp(r.entries.slot(j).elems()),
            |r, i, j| {
                r.entries.swap(i, j);
                r.refs.swap(i, j);
            });
    }

    /// The slot holding @e, if any.
    fn find(&self, e: &Entry) -> Option<usize> {
        verify_entry(e);
        let want = e.as_bytes();
        if want.len() > self.entries.stride() {
            return None;
        }
        Eytzinger0::<usize>::new(self.len()).find(|i| self.entries.slot(i).elems()[..want.len()].cmp(want))
    }

    /// A copy, @e added - with room for it, if it's bigger than any: sorted.
    fn with(&self, e: &Entry) -> Result<Self, AllocError> {
        let e = e.as_bytes();
        let mut new = ReplicasCpu {
            entries: self.entries.restrided(self.entries.stride().max(e.len()), 1)?,
            refs:    Vec::with_capacity(self.len() + 1)?,
        };
        for r in self.refs.iter() {
            new.refs.push(AtomicI32::new(r.load(Ordering::Relaxed)))?;
        }
        new.refs.push(AtomicI32::new(0))?;
        new.entries.slot_mut(self.len()).elems_mut()[..e.len()].copy_from_slice(e);
        new.sort();
        Ok(new)
    }

    /// A copy of those @keep says to: sorted.
    fn filtered(&self, keep: impl Fn(&Entry) -> bool) -> Result<Self, AllocError> {
        let mut new = Self::zeroed(self.entries().filter(|e| keep(e)).count(), self.entries.stride())?;
        for (dst, i) in (0..self.len()).filter(|&i| keep(self.entry(i))).enumerate() {
            new.entries.slot_mut(dst).elems_mut().copy_from_slice(self.entries.slot(i).elems());
            new.refs[dst].store(self.refs[i].load(Ordering::Relaxed), Ordering::Relaxed);
        }
        new.sort();
        Ok(new)
    }

    /// Those @keep says to, the rest dropped. Whether any were.
    fn retain(&mut self, keep: impl Fn(&Entry, &AtomicI32) -> bool) -> bool {
        let mut dst = 0;
        for i in 0..self.len() {
            if keep(self.entry(i), &self.refs[i]) {
                self.entries.copy(i, dst);
                self.refs.swap(i, dst);
                dst += 1;
            }
        }

        let changed = dst != self.len();
        if changed {
            self.entries.truncate(dst);
            self.refs.truncate(dst);
            self.sort();
        }
        changed
    }

    /// Slot @i dropped: the last moved into it.
    fn remove(&mut self, i: usize) {
        let last = self.len() - 1;
        self.entries.copy(last, i);
        self.refs.swap(i, last);
        self.entries.truncate(last);
        self.refs.truncate(last);
        self.sort();
    }

    /// The superblock's replicas section made ours: replicas_v0, which older
    /// versions read, if every entry's nr_required is 1. False if there's no
    /// room.
    fn to_sb(&self, sb: &mut (impl SbResize + ?Sized)) -> bool {
        /// Section @F, resized, the other one deleted: its entries' bytes.
        fn section<'a, F: SbSection<Arm: Unsized + 'a> + ?Sized>(sb: &'a mut (impl SbResize + ?Sized), u64s: u32,
                                                               other: SbFieldType) -> Option<&'a mut [u8]> {
            sb.field_resize::<F>(u64s)?;
            sb.field_delete(other);
            Some(&mut bytes_of_mut(sb.field_mut::<F>().expect("just resized"))[SECTION_HEAD..])
        }

        /// @e as the section has it: v0's entries have no nr_required, the
        /// byte before the devices.
        fn parts(e: &Entry, v1: bool) -> [&[u8]; 2] {
            let b = e.as_bytes();
            if v1 { [b, &[]] } else { [&b[..DEVS - 1], &b[DEVS..]] }
        }

        let v1 = self.entries().any(|e| e.nr_required != 1);

        let bytes = SECTION_HEAD + self.entries().flat_map(|e| parts(e, v1)).map(<[u8]>::len).sum::<usize>();
        let u64s = bytes.div_ceil(size_of::<u64>()) as u32;
        let out = if v1 {
            section::<SbFieldReplicas>(sb, u64s, SbFieldType::replicas_v0)
        } else {
            section::<SbFieldReplicasV0>(sb, u64s, SbFieldType::replicas)
        };
        let Some(out) = out else { return false };

        out.fill(0);
        let mut at = 0;
        for p in self.entries().flat_map(|e| parts(e, v1)) {
            out[at..at + p.len()].copy_from_slice(p);
            at += p.len();
        }
        true
    }

    /// to_sb(), no room an error.
    fn write_sb(&self, fs: &Fs, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        fs.ensure(self.to_sb(&mut fs.sb_resize(sb)), Errcode::ENOSPC_sb_replicas)
    }

    /// Validated as a section is, its entries against @sb's members: none
    /// twice.
    fn validate<S: SbState>(&mut self, sb: &Sb<S>, err: &mut Printbuf) -> Result<(), BchError> {
        self.sort();

        let mut prev: Option<usize> = None;
        for i in Eytzinger0::<usize>::new(self.len()).iter() {
            let e = self.entry(i);
            e.validate(|d| member_exists(sb, d.into()), err)?;

            if let Some(p) = prev.filter(|&p| self.entries.slot(p).elems() == self.entries.slot(i).elems()) {
                write!(err, "duplicate replicas entry ");
                self.entry(p).to_text(err);
                return Err(Errcode::invalid_sb_replicas.into());
            }
            prev = Some(i);
        }
        Ok(())
    }
}

impl ToText for ReplicasCpu {
    fn to_text(&self, out: &mut Printbuf) {
        prt_spaced(out, self.with_refs(), |out, (e, r)| {
            write!(out, "ref={} ", r.load(Ordering::Relaxed) as u32);
            e.to_text(out);
        });
    }
}

/// C's WARN(), with what it says logged.
fn warn(fs: &Fs, msg: fmt::Arguments<'_>) {
    crate::warn_on!(true);
    crate::bch_err!(fs, "{msg}");
}

/// An entry as one being searched for must be: devices, sorted, and
/// nr_required fewer than them, but 1 - checked with CONFIG_BCACHEFS_DEBUG.
fn verify_entry(_e: &Entry) {
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    {
        let e = _e;
        assert!(!e.devs.is_empty());
        assert!(!(e.nr_required > 1 && usize::from(e.nr_required) >= e.devs.len()));
        assert!(e.devs.windows(2).all(|w| u32::from(w[0]) == BCH_SB_MEMBER_INVALID || w[0] < w[1]));
    }
}

/// A section's entries, @walk, validated as the section: against @sb's
/// members, none twice.
fn section_validate<'a, I: Iterator<Item = SbEntry<'a>>>(sb: &Sb<Unvalidated>, walk: impl Fn() -> I,
                                                         err: &mut Printbuf) -> Result<(), BchError> {
    ReplicasCpu::from_entries(walk, |_| true)
        .map_err(|_| BchError::from(Errcode::ENOMEM_cpu_replicas))?
        .validate(sb, err)
}

fn section_to_text<'a>(out: &mut Printbuf, entries: impl Iterator<Item = SbEntry<'a>>) {
    prt_spaced(out, entries, |out, e| e.to_text(out));
    out.newline();
}

impl SbFieldOps for SbFieldReplicas<Validated> {
    fn validate(sb: &Sb<Unvalidated>, f: &SbFieldReplicas<Unvalidated>,
                _flags: ValidateFlags, err: &mut Printbuf) -> Result<(), BchError> {
        section_validate(sb, || v1_entries(f), err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &Sb<Unvalidated>,
                f: &SbFieldReplicas<Unvalidated>) {
        section_to_text(out, v1_entries(f))
    }

    /// The table, from this section or replicas_v0: at open - after, it
    /// would lose the refs.
    fn to_cpu(fs: &Fs, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        assert!(!fs.flags.get(FsFlag::started), "replicas table rebuilt with refs held");

        let mut new = ReplicasCpu::from_entries(|| Validated::sb_entries(sb.sb()), |t| !fs.replicas_obsolete(t))
            .map_err(|_| BchError::from(Errcode::ENOMEM_cpu_replicas))?;
        new.sort();
        *fs.replicas.write() = new;
        Ok(())
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<ReplicasEntryV1<Unvalidated>>(b, SECTION_HEAD, |e| e.data_type != DataType::free)
    }
}

impl SbFieldOps for SbFieldReplicasV0<Validated> {
    fn validate(sb: &Sb<Unvalidated>, f: &SbFieldReplicasV0<Unvalidated>,
                _flags: ValidateFlags, err: &mut Printbuf) -> Result<(), BchError> {
        section_validate(sb, || v0_entries(f), err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &Sb<Unvalidated>,
                f: &SbFieldReplicasV0<Unvalidated>) {
        section_to_text(out, v0_entries(f))
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<ReplicasEntryV0<Unvalidated>>(b, SECTION_HEAD, |e| e.data_type != DataType::free)
    }
}

// ── Marking, refs, gc ────────────────────────────────────────────────────

impl Fs {
    /// @r's slot @i dropped, and the superblock's entries rewritten without
    /// it - not written: the caller's.
    fn replicas_kill(&self, r: &mut ReplicasCpu, sb: &mut SbLockGuard<'_>, i: usize) {
        r.remove(i);
        if let Err(e) = r.write_sb(self, sb) {
            warn(self, format_args!("replicas write_sb() error: {e}"));
        }
    }

    /// Whether entries of @data_type have no place in the superblock: user
    /// data's don't, past no_sb_user_data_replicas.
    fn replicas_obsolete(&self, data_type: DataType) -> bool {
        data_type > DataType::btree &&
            self.sb.version_incompat.get() >= MetadataVersion::no_sb_user_data_replicas
    }

    /// Whether @e is marked in the superblock - or needn't be: obsolete, or
    /// of no devices.
    pub fn replicas_marked(&self, e: &Entry) -> bool {
        self.replicas_obsolete(e.data_type.get()) || e.devs.is_empty() || self.replicas.read().find(e).is_some()
    }

    /// @e added, if it isn't there, and written to the superblock - and
    /// @refs taken on it.
    #[cold]
    #[inline(never)]
    fn mark_replicas_slowpath(&self, e: &Entry, refs: i32) -> Result<(), BchError> {
        verify_entry(e);

        let mut sb = self.sb_lock();
        let dirty = {
            let mut r = self.replicas.write();
            let mut dirty = false;
            if r.find(e).is_none() {
                let new = r.with(e).map_err(|_| self.err(Errcode::ENOMEM_cpu_replicas))?;
                new.write_sb(self, &mut sb)?;
                *r = new;
                dirty = true;
            }
            let i = r.find(e).expect("an entry just added");
            r.refs[i].fetch_add(refs, Ordering::Relaxed);
            dirty
        };
        // write_super() takes the replicas lock for read: after it's dropped
        if dirty {
            self.write_super_replicas(&mut sb);
        }
        Ok(())
    }

    /// @e marked in the superblock, if it isn't.
    pub fn mark_replicas(&self, e: &Entry) -> Result<(), BchError> {
        if likely(self.replicas_marked(e)) { Ok(()) } else { self.mark_replicas_slowpath(e, 0) }
    }

    /// Whether @e, a journal entry, has devices: only those are refcounted.
    fn journal_ref_entry(e: &Entry) -> bool {
        if e.devs.is_empty() {
            return false;
        }
        assert_eq!(e.data_type.get(), DataType::journal,
                   "a ref on replicas entry of data type {}", e.data_type.get());
        verify_entry(e);
        true
    }

    /// A ref on journal entry @e - marked, if it isn't.
    pub fn replicas_entry_get(&self, e: &Entry) -> Result<(), BchError> {
        if !Self::journal_ref_entry(e) {
            return Ok(());
        }

        let got = {
            let r = self.replicas.read();
            r.find(e).map(|i| r.refs[i].fetch_add(1, Ordering::Relaxed)).is_some()
        };
        if got { Ok(()) } else { self.mark_replicas_slowpath(e, 1) }
    }

    /// @nr refs on journal entry @e put: the last killing it, and writing the
    /// superblock without it.
    pub fn replicas_entry_put_many(&self, e: &Entry, nr: u32) {
        if !Self::journal_ref_entry(e) {
            return;
        }

        {
            let r = self.replicas.read();
            match r.find(e).map(|i| r.refs[i].fetch_sub(nr as i32, Ordering::Relaxed) - nr as i32) {
                Some(v) if v > 0 => return,
                Some(0) => {}
                _ => {
                    cold_path();
                    warn(self, format_args!("refcount error putting {e}"));
                    return;
                }
            }
        }

        let mut sb = self.sb_lock();
        {
            let mut r = self.replicas.write();
            if let Some(i) = r.find(e).filter(|&i| r.refs[i].load(Ordering::Relaxed) == 0) {
                self.replicas_kill(&mut r, &mut sb, i);
            }
        }
        self.write_super_replicas(&mut sb);
    }

    /// Mount-time reconciliation, not a runtime gc - at runtime the last
    /// replicas_entry_put_many() kills an entry and writes the superblock.
    ///
    /// The superblock's replicas are read before what the journal holds is
    /// known; journal_read() then refs every entry it finds. What's left at
    /// zero is journal the superblock claims and the disks don't have -
    /// written while other devices were offline, since reclaimed - and it
    /// would refuse operations (can_read_fs_with_devs()) forever.
    ///
    /// Journal entries only: nothing else is refcounted - see
    /// replicas_gc_accounted().
    pub fn replicas_gc_reffed(&self) -> Result<(), BchError> {
        let mut sb = self.sb_lock();
        let dirty = {
            let mut r = self.replicas.write();
            let changed = r.retain(|e, refs| e.data_type != DataType::journal ||
                                             refs.load(Ordering::Relaxed) != 0);
            if changed {
                r.write_sb(self, &mut sb)?;
            }
            changed
        };
        if dirty {
            self.write_super(&mut sb);
        }
        Ok(())
    }

    /// Entries that no accounting is kept for dropped - journal entries
    /// aside, which have none.
    pub fn replicas_gc_accounted(&self) -> Result<(), BchError> {
        accounting_mem::mem_gc(self);

        let mut sb = self.sb_lock();
        {
            let mark = self.mark_lock().write();
            let mut r = self.replicas.write();

            let new = r.filtered(|e| e.data_type == DataType::journal ||
                    accounting_mem::has_replicas(self, &mark, e))
                .map_err(|_| {
                    crate::bch_err!(self, "error allocating c->replicas_gc");
                    self.err(Errcode::ENOMEM_replicas_gc)
                })?;
            new.write_sb(self, &mut sb)?;
            *r = new;
        }
        self.write_super(&mut sb);
        Ok(())
    }

    /// Whether the filesystem can be read with @devs: every entry, as
    /// can_read_with_devs(). One that can't be is in @err with the sectors
    /// accounted to it, once accounting's been read - before, at mount, there
    /// are none to say: none, and it's a stale entry refusing.
    pub fn can_read_fs_with_devs(&self, devs: &DevsMask, flags: ForceIf, mut err: Option<&mut Printbuf>) -> bool {
        let accounted = self.recovery.runner.pass_done.get() >= RecoveryPass::accounting_read;
        // mark_lock, for the sectors: before the table's lock, the order above
        let mark = (err.is_some() && accounted).then(|| self.mark_lock().read());

        self.replicas.read().entries()
            .filter(|e| !self.replicas_obsolete(e.data_type.get()))
            .all(|e| {
                let ok = can_read_with_devs(e, devs, flags, err.as_deref_mut());
                if let (false, Some(err), Some(mark)) = (ok, err.as_deref_mut(), &mark) {
                    if let Some(pos) = try_replicas_acct(e.data_type.get(), e.nr_required, &e.devs) {
                        let v = accounting_mem::mem_read_locked(self, mark, &pos);
                        write!(err, "  with {} sectors accounted to it\n", ReplicasCounters::from_slice(&v).sectors);
                    }
                }
                ok
            })
    }
}

impl Fs {
    /// The caller writes the superblock, after dropping mark_lock.
    pub(crate) fn replicas_entry_kill(&self, sb: &mut SbLockGuard<'_>, _mark: &MarkWrite<'_>, e: &Entry) {
        let mut r = self.replicas.write();
        match r.find(e) {
            Some(i) => self.replicas_kill(&mut r, sb, i),
            None => warn(self, format_args!("replicas entry not found in sb: {e}")),
        }
    }
}

impl Fs {
    /// A warning for any ref left: at shutdown, the journal done with them all.
    pub fn verify_replicas_refs_clean(&self) {
        for (e, r) in self.replicas.read().with_refs().filter(|(_, r)| r.load(Ordering::Relaxed) != 0) {
            warn(self, format_args!("replicas entry ref leaked:\nref={} {e}", r.load(Ordering::Relaxed) as u32));
        }
    }
}

impl Fs {
    /// The table emptied: for an image stripped of its replicas sections.
    pub fn replicas_clear(&self) {
        *self.replicas.write() = ReplicasCpu::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::dev::SbHandle;

    const JOURNAL: DataType = DataType::journal;
    const BTREE: DataType = DataType::btree;
    const USER: DataType = DataType::user;

    fn entry(data_type: DataType, nr_required: u8, devs: &[u8]) -> ReplicasPadded {
        let mut r = ReplicasPadded::default();
        EntryBuf(&mut r).fill(data_type, nr_required, devs);
        r
    }

    fn same(a: &ReplicasCpu, b: &ReplicasCpu) -> bool {
        a.len() == b.len() && a.entries().all(|e| b.find(e).is_some())
    }

    /// What's added is found, what's removed isn't, and an entry that's
    /// another's prefix isn't it; the superblock gets them back - as v0
    /// while every nr_required is 1.
    #[test]
    fn table() {
        let es = [entry(JOURNAL, 1, &[0, 1]), entry(BTREE, 1, &[2]), entry(USER, 1, &[0, 1, 2]),
                  entry(BTREE, 1, &[0, 1])];
        let mut r = ReplicasCpu::new();
        for e in &es {
            r = r.with(e).unwrap();
        }
        assert!(es.iter().all(|e| r.find(e).is_some()));
        assert!(r.find(&entry(BTREE, 1, &[0])).is_none());

        r.remove(r.find(&es[1]).unwrap());
        assert!(r.find(&es[1]).is_none());
        assert!([&es[0], &es[2], &es[3]].iter().all(|e| r.find(e).is_some()));

        let mut sb = SbHandle::default();
        sb.sb_realloc(0).unwrap();
        let back = |sb: &SbHandle| {
            let mut b = ReplicasCpu::from_entries(|| Validated::sb_entries(sb.sb()), |_| true).unwrap();
            b.sort();
            b
        };

        assert!(r.to_sb(&mut sb));
        assert!(sb.field::<SbFieldReplicasV0>().is_some());
        assert!(same(&r, &back(&sb)));

        r = r.with(&entry(USER, 2, &[0, 1, 2])).unwrap();
        assert!(r.to_sb(&mut sb));
        assert!(sb.field::<SbFieldReplicas>().is_some());
        assert!(sb.field::<SbFieldReplicasV0>().is_none());
        assert!(same(&r, &back(&sb)));
    }

    /// A section listing an entry twice is invalid.
    #[test]
    fn duplicate() {
        let invalid = BCH_SB_MEMBER_INVALID as u8;
        let mut sb = SbHandle::default();
        sb.sb_realloc(0).unwrap();

        let e = entry(BTREE, 1, &[invalid]);
        let mut r = ReplicasCpu::new().with(&e).unwrap();
        let mut err = Printbuf::new();
        assert!(r.validate(sb.sb(), &mut err).is_ok());

        let mut r = r.with(&e).unwrap();
        assert!(r.validate(sb.sb(), &mut err).unwrap_err().matches(Errcode::invalid_sb_replicas));
    }

    /// An entry cut short at a section's end - C's walk stopped there - is
    /// dropped at read with a warning, the rest of its u64 zeroed; at write,
    /// it's invalid.
    #[test]
    fn partial_entry() {
        type V0 = SbFieldReplicasV0;
        let validate = |write: bool| {
            let invalid = BCH_SB_MEMBER_INVALID as u8;
            let mut sb = SbHandle::default();
            sb.sb_realloc(0).unwrap();
            assert!(ReplicasCpu::new().with(&entry(BTREE, 1, &[invalid])).unwrap().to_sb(&mut sb));

            // a u64 more: past the whole entry, 3 bytes, the start of one
            // claiming 20 devices
            let f = sb.field_resize::<V0>(2).unwrap();
            bytes_of_mut(f)[SECTION_HEAD + 3..][..2].copy_from_slice(&[BTREE.0 as u8, 20]);

            let flags = if write { ValidateFlags::write } else { ValidateFlags::empty() };
            let mut err = Printbuf::new();
            let ret = crate::errcode::ret_to_c(crate::sb::io::validate_section::<V0>(&mut sb, flags, &mut err));
            (sb, ret, err)
        };

        let (sb, ret, err) = validate(false);
        assert_eq!((ret, err.as_str()), (0, "13 bytes past its last whole entry, dropped"));
        let f = sb.field::<V0>().unwrap();
        assert_eq!(f.as_bytes().len(), 8);
        assert!(f.as_bytes()[SECTION_HEAD + 3..].iter().all(|&b| b == 0));
        assert_eq!(Validated::sb_entries(sb.sb()).count(), 1);

        let (_, ret, err) = validate(true);
        assert!(ret != 0);
        assert_eq!(err.as_str(), "13 bytes past its last whole entry");
    }
}
