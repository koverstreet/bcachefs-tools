// SPDX-License-Identifier: GPL-2.0

//! Replicas: which sets of devices hold data of which type, so the filesystem
//! knows what it can still read with devices missing.
//!
//! An entry is a data type, how many of its devices are required to read it
//! - nr_required, 0 for erasure coded - and the devices, sorted:
//! bch_replicas_entry_v1, a #[counted] DST. The superblock lists them in the
//! replicas section - or replicas_v0, whose entries have no nr_required,
//! when every entry's would be 1 - back to back, each as long as its device
//! list, the section zero padded after the last.
//!
//! An entry being built - from a key, a device list - is written into room
//! for a key's worth of devices, BCH_BKEY_PTRS_MAX: C's bch_replicas_padded,
//! or a disk_accounting_pos, whose replicas entry is that size exactly.

use crate::btree::bkey::{BkeySC, BkeyV};
use crate::c;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::ec::trigger::stripe_block_ptrs;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::opts::prt_data_type;
use crate::sb::io::{entries_as_read, entries_end, sb_field_get, SbField, SbFieldOps, SbState};
use crate::sb::members::member_exists;
use crate::util::ffi::Opaque;
use crate::util::rcu::read_lock;
use crate::util::alloc::{flags, AllocError, KVVec};
use crate::util::eytzinger::Eytzinger0;
use crate::util::strided::StridedBuf;
use crate::util::vstructs::{bytes_of, bytes_of_mut, from_ffi, from_ffi_mut, Head, Unsized, Unvalidated,
                            Validated, ValidatedEntries};
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_uint};
use core::fmt;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicI32, Ordering};
use cstruct_macros::c_export;

type Entry = c::bch_replicas_entry_v1<Validated>;

/// Where an entry's devices start: after data_type, nr_devs, nr_required.
const DEVS: usize = offset_of!(Head<c::bch_replicas_entry_v1>, devs);

/// Where a replicas section's entries start - either section's.
const SECTION_HEAD: usize = size_of::<Head<c::bch_sb_field_replicas>>();
const _: () = assert!(SECTION_HEAD == size_of::<Head<c::bch_sb_field_replicas_v0>>());

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
    pub data_type:   u8,
    pub nr_required: Option<u8>,
    pub devs:        &'a [u8],
}

impl<'a> From<&'a Entry> for SbEntry<'a> {
    fn from(e: &'a Entry) -> Self {
        SbEntry { data_type: e.data_type, nr_required: Some(e.nr_required), devs: &e.devs }
    }
}

impl<'a> From<&'a c::bch_replicas_entry_v0<Validated>> for SbEntry<'a> {
    fn from(e: &'a c::bch_replicas_entry_v0<Validated>) -> Self {
        SbEntry { data_type: e.data_type, nr_required: None, devs: &e.devs }
    }
}

impl<'a> From<&'a c::bch_replicas_entry_v1<Unvalidated>> for SbEntry<'a> {
    fn from(e: &'a c::bch_replicas_entry_v1<Unvalidated>) -> Self {
        SbEntry { data_type: e.data_type, nr_required: Some(e.nr_required), devs: &e.devs }
    }
}

impl<'a> From<&'a c::bch_replicas_entry_v0<Unvalidated>> for SbEntry<'a> {
    fn from(e: &'a c::bch_replicas_entry_v0<Unvalidated>) -> Self {
        SbEntry { data_type: e.data_type, nr_required: None, devs: &e.devs }
    }
}

impl SbEntry<'_> {
    fn to_text(&self, out: &mut Printbuf) {
        prt_data_type(out, c::bch_data_type(self.data_type.into()));
        match self.nr_required {
            Some(r) => write!(out, ": {r}/{} [", self.devs.len()),
            None    => write!(out, ": {} [", self.devs.len()),
        };
        prt_spaced(out, self.devs, |out, d| write!(out, "{d}"));
        write!(out, "]");
    }
}

impl ToText for Entry {
    fn to_text(&self, out: &mut Printbuf) {
        SbEntry::from(self).to_text(out)
    }
}
crate::display_via_to_text!(Entry);

impl Entry {
    /// Devices, nr_required fewer than them - or 1, a single device's - and
    /// none that @dev_ok says isn't one.
    fn validate(&self, dev_ok: impl Fn(u8) -> bool, err: &mut Printbuf) -> Result<(), BchError> {
        let invalid = |err: &mut Printbuf, msg: fmt::Arguments<'_>| {
            write!(err, "{msg} in entry ");
            self.to_text(err);
            Err(bch_errcode::BCH_ERR_invalid_replicas_entry.into())
        };

        if self.devs.is_empty() {
            return invalid(err, format_args!("no devices"));
        }
        if self.nr_required > 1 && usize::from(self.nr_required) >= self.devs.len() {
            return invalid(err, format_args!("bad nr_required"));
        }
        for &d in &self.devs {
            if u32::from(d) != c::BCH_SB_MEMBER_INVALID && !dev_ok(d) {
                return invalid(err, format_args!("invalid device {d}"));
            }
        }
        Ok(())
    }
}

/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_replicas_entry_to_text(out: &mut Printbuf, e: *const Head<c::bch_replicas_entry_v1>) {
    // SAFETY: the caller's
    unsafe { from_ffi::<Entry>(e) }.to_text(out)
}

/// Against the devices the filesystem has.
///
/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_replicas_entry_validate(e: *const Head<c::bch_replicas_entry_v1>, c: &Opaque<c::bch_fs>,
                                           err: &mut Printbuf) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: the caller's
    let e = unsafe { from_ffi::<Entry>(e) };
    ret_to_c(e.validate(|d| fs.dev_exists(d.into()), err))
}

/// Its devices in order.
///
/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_replicas_entry_sort(e: *mut Head<c::bch_replicas_entry_v1>) {
    // SAFETY: the caller's
    unsafe { from_ffi_mut::<Entry>(e) }.devs.sort_unstable();
}

/// An entry being built, in room for a key's worth of devices.
struct EntryBuf<'a>(&'a mut [u8; DEVS + c::BCH_BKEY_PTRS_MAX as usize]);

impl EntryBuf<'_> {
    /// # Safety
    /// @e has room for BCH_BKEY_PTRS_MAX devices.
    unsafe fn from_c<'a>(e: *mut Head<c::bch_replicas_entry_v1>) -> EntryBuf<'a> {
        // SAFETY: the caller's - and any bytes are an entry's
        EntryBuf(unsafe { &mut *e.cast() })
    }

    fn start(&mut self, data_type: c::bch_data_type, nr_required: u8) {
        self.0[..DEVS].copy_from_slice(&[data_type.0 as u8, 0, nr_required]);
    }

    fn set_nr_required(&mut self, nr_required: u8) {
        self.0[DEVS - 1] = nr_required;
    }

    /// replicas_entry_add_dev(): past the room, a bug - a key has no more
    /// pointers than that.
    fn add_dev(&mut self, dev: u8) {
        let nr = usize::from(self.0[1]);
        self.0[DEVS + nr] = dev;
        self.0[1] += 1;
    }

    fn sort(&mut self) {
        let nr = usize::from(self.0[1]);
        self.0[DEVS..DEVS + nr].sort_unstable();
    }
}

/// The entry for @k's data: its pointers' devices, the cached ones aside -
/// a pointer into a stripe makes it erasure coded, nr_required 0 - or a
/// stripe's blocks'. A key with no data leaves the data type as it was, and
/// no devices.
fn bkey_to_replicas(fs: &Fs, e: &mut EntryBuf<'_>, k: BkeySC<'_>) {
    type K = c::bch_bkey_type;
    type D = c::bch_data_type;

    let data_type = match c::bch_bkey_type(k.k.type_.into()) {
        K::KEY_TYPE_btree_ptr | K::KEY_TYPE_btree_ptr_v2   => Some(D::BCH_DATA_btree),
        K::KEY_TYPE_extent | K::KEY_TYPE_reflink_v         => Some(D::BCH_DATA_user),
        K::KEY_TYPE_stripe                                => Some(D::BCH_DATA_parity),
        _                                                 => None,
    };
    e.0[1] = 0;

    match data_type {
        Some(D::BCH_DATA_parity) => {
            let s = k.arm::<c::Stripe>().expect("a stripe");
            e.start(D::BCH_DATA_parity, s.v().nr_blocks.wrapping_sub(s.v().nr_redundant));
            for ptr in stripe_block_ptrs(s) {
                e.add_dev(ptr.dev());
            }
        }
        Some(t) => {
            e.start(t, 1);
            for p in k.arm::<Ptrs>().expect("a key with pointers").ptrs_decoded(fs) {
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
        None => {}
    }
    e.sort();
}

/// # Safety
/// @e has room for BCH_BKEY_PTRS_MAX devices.
#[c_export]
pub unsafe fn bch2_bkey_to_replicas(c: &Opaque<c::bch_fs>, e: *mut Head<c::bch_replicas_entry_v1>, k: BkeySC<'_>) {
    // SAFETY: the caller's
    bkey_to_replicas(Fs::from_c(c), &mut unsafe { EntryBuf::from_c(e) }, k)
}

/// The entry for data of @data_type on @devs.
///
/// # Safety
/// @e has room for BCH_BKEY_PTRS_MAX devices.
#[c_export]
pub unsafe fn bch2_devlist_to_replicas(e: *mut Head<c::bch_replicas_entry_v1>, data_type: c::bch_data_type,
                                       devs: c::bch_devs_list) {
    assert!(data_type.0 != 0 &&
            data_type != c::bch_data_type::BCH_DATA_sb &&
            data_type.0 < c::BCH_DATA_NR,
            "replicas entry of data type {}", data_type.0);

    // SAFETY: the caller's
    let mut e = unsafe { EntryBuf::from_c(e) };
    e.start(data_type, 1);
    for &d in devs.as_slice() {
        e.add_dev(d);
    }
    e.sort();
}

// ── The superblock's entries ─────────────────────────────────────────────

/// A replicas section's entries, as far as they go: its end, or the zeroes
/// after the last.
fn v1_entries(f: &c::bch_sb_field_replicas) -> impl Iterator<Item = SbEntry<'_>> {
    ValidatedEntries::<Entry>::new(&f.as_bytes()[SECTION_HEAD..]).map(SbEntry::from)
        .take_while(|e| e.data_type != 0)
}

/// v1_entries(), of replicas_v0.
fn v0_entries(f: &c::bch_sb_field_replicas_v0) -> impl Iterator<Item = SbEntry<'_>> {
    ValidatedEntries::<c::bch_replicas_entry_v0<Validated>>::new(&f.as_bytes()[SECTION_HEAD..]).map(SbEntry::from)
        .take_while(|e| e.data_type != 0)
}

/// v1_entries(), of a section as read.
fn v1_entries_as_read(b: &[u8]) -> impl Iterator<Item = SbEntry<'_>> {
    entries_as_read::<c::bch_sb_field_replicas, c::bch_replicas_entry_v1<Unvalidated>>(b, SECTION_HEAD)
        .map(SbEntry::from)
}

/// v1_entries_as_read(), of replicas_v0.
fn v0_entries_as_read(b: &[u8]) -> impl Iterator<Item = SbEntry<'_>> {
    entries_as_read::<c::bch_sb_field_replicas_v0, c::bch_replicas_entry_v0<Unvalidated>>(b, SECTION_HEAD)
        .map(SbEntry::from)
}

/// The superblock's replicas entries: replicas', if it has the section, else
/// replicas_v0's.
pub fn sb_entries(sb: &c::bch_sb) -> impl Iterator<Item = SbEntry<'_>> {
    let v1 = sb_field_get::<c::bch_sb_field_replicas>(sb).map(v1_entries);
    let v0 = v1.is_none().then(|| sb_field_get::<c::bch_sb_field_replicas_v0>(sb)).flatten().map(v0_entries);

    v1.into_iter().flatten().chain(v0.into_iter().flatten())
}

impl c::bch_sb {
    /// Whether it lists journal replicas.
    pub fn has_journal(&self) -> bool {
        sb_entries(self).any(|e| u32::from(e.data_type) == c::bch_data_type::BCH_DATA_journal.0)
    }

    /// The data types with data on @dev: a mask, by bch_data_type.
    pub fn dev_has_data(&self, dev: u32) -> u32 {
        sb_entries(self)
            .filter(|e| e.data_type < 32 && e.devs.iter().any(|&d| u32::from(d) == dev))
            .fold(0, |m, e| m | 1 << e.data_type)
    }
}

/// # Safety
/// @sb is validated.
#[c_export]
pub unsafe fn bch2_sb_has_journal(sb: *const Head<c::bch_sb>) -> bool {
    // SAFETY: the caller's
    unsafe { c::bch_sb::<Validated>::from_ffi(sb) }.has_journal()
}

/// # Safety
/// @sb is validated.
#[c_export]
pub unsafe fn bch2_sb_dev_has_data(sb: *const Head<c::bch_sb>, dev: c_uint) -> c_uint {
    // SAFETY: the caller's
    unsafe { c::bch_sb::<Validated>::from_ffi(sb) }.dev_has_data(dev)
}

#[c_export]
pub fn bch2_dev_has_data(_c: &Opaque<c::bch_fs>, ca: &c::bch_dev) -> c_uint {
    ca.has_data()
}

// ── Can we read, can we write ────────────────────────────────────────────

/// Whether data that @e lists can be read with @devs: none of its devices
/// missing past what @flags, BCH_FORCE_IF_*, allows - degraded, or lost. If
/// not, why in @err.
fn can_read_with_devs(e: &Entry, devs: &c::bch_devs_mask, flags: u32, err: Option<&mut Printbuf>) -> bool {
    type D = c::bch_data_type;

    if u32::from(e.data_type) == D::BCH_DATA_cached.0 {
        return true;
    }

    let invalid = e.devs.iter().filter(|&&d| u32::from(d) == c::BCH_SB_MEMBER_INVALID).count();
    let online = e.devs.iter()
        .filter(|&&d| u32::from(d) != c::BCH_SB_MEMBER_INVALID && devs.d.test(d.into()))
        .count();

    if invalid == e.devs.len() {
        return true;
    }

    let metadata = u32::from(e.data_type) < D::BCH_DATA_user.0;
    let mut dflags = 0;
    if online < usize::from(e.nr_required) {
        dflags |= if metadata { c::BCH_FORCE_IF_METADATA_LOST } else { c::BCH_FORCE_IF_DATA_LOST };
    }
    if online + invalid < e.devs.len() {
        dflags |= if metadata { c::BCH_FORCE_IF_METADATA_DEGRADED } else { c::BCH_FORCE_IF_DATA_DEGRADED };
    }

    if dflags & !flags != 0 {
        if let Some(err) = err {
            write!(err, "insufficient devices online ({online}) for replicas entry ");
            e.to_text(err);
            err.newline();
        }
        return false;
    }
    true
}

/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_can_read_replicas_with_devs(_c: &Opaque<c::bch_fs>, devs: &c::bch_devs_mask,
                                               e: *const Head<c::bch_replicas_entry_v1>, flags: c_uint,
                                               err: Option<&mut Printbuf>) -> bool {
    // SAFETY: the caller's
    can_read_with_devs(unsafe { from_ffi::<Entry>(e) }, devs, flags, err)
}

/// Whether the filesystem can write with @devs - at start, the rw set; for a
/// device leaving rw, the rw set without it: somewhere for journal, btree and
/// user data at all, and leaving rw, no fewer replicas than configured of
/// each - than there are now, if that's fewer - unless @flags allows going
/// degraded. If not, why in @err.
fn can_write_with_devs(fs: &Fs, devs: &c::bch_devs_mask, check: c::bch_write_check,
                       flags: u32, err: &mut Printbuf) -> bool {
    type D = c::bch_data_type;

    // have: what the rw set has now; online: what @devs would have
    let mut have = [0u32; c::BCH_DATA_NR as usize];
    let mut online = [0u32; c::BCH_DATA_NR as usize];

    let rcu = read_lock();
    for ca in fs.devs_rcu_in(fs.allocator.rw_devs[0].iter(), &rcu) {
        let durability = u32::from(ca.mi.durability);
        if durability == 0 {
            continue;
        }

        let is_online = devs.d.test(ca.dev_idx.into());
        for (i, (h, o)) in have.iter_mut().zip(&mut online).enumerate() {
            if u32::from(ca.mi.data_allowed) & 1 << i != 0 {
                *h += durability;
                *o += durability * is_online as u32;
            }
        }
    }
    drop(rcu);

    let (j, b, u) = (D::BCH_DATA_journal.0 as usize, D::BCH_DATA_btree.0 as usize, D::BCH_DATA_user.0 as usize);

    for (t, name) in [(j, "journal"), (b, "btree"), (u, "user data")] {
        if online[t] == 0 {
            write!(err, "No rw {name} devices online\n");
            return false;
        }
    }

    if check == c::bch_write_check::BCH_WRITE_CHECK_start {
        return true;
    }

    let opts = fs.opts();
    let journal_replicas = match opts.journal_replicas {
        0 => opts.metadata_replicas,
        r => r,
    };
    let want = |t: usize, force: u32, name: &str, replicas: u8, err: &mut Printbuf| {
        if flags & force == 0 && online[t] < have[t] && online[t] < u32::from(replicas) {
            write!(err, "Insufficient rw {name} devices ({} < {replicas}) online\n", online[t]);
            return false;
        }
        true
    };

    want(j, c::BCH_FORCE_IF_METADATA_DEGRADED, "journal", journal_replicas, err) &&
    want(b, c::BCH_FORCE_IF_METADATA_DEGRADED, "btree", opts.metadata_replicas, err) &&
    want(u, c::BCH_FORCE_IF_DATA_DEGRADED, "user data", opts.data_replicas, err)
}

#[c_export]
pub fn bch2_can_write_fs_with_devs(c: &Opaque<c::bch_fs>, devs: c::bch_devs_mask, check: c::bch_write_check,
                                   flags: c_uint, err: &mut Printbuf) -> bool {
    can_write_with_devs(Fs::from_c(c), &devs, check, flags, err)
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
    entries: StridedBuf,
    refs:    KVVec<AtomicI32>,
}

impl ReplicasCpu {
    pub const fn new() -> Self {
        ReplicasCpu { entries: StridedBuf::new(), refs: KVVec::new() }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn entry(&self, i: usize) -> &Entry {
        // SAFETY: a slot holds an entry, as long as its nr_devs says - the
        // stride is the biggest's - written there by from_entries() or with()
        unsafe { from_ffi::<Entry>(self.entries.slot(i).as_ptr().cast()) }
    }

    fn entries(&self) -> impl Iterator<Item = &Entry> {
        (0..self.len()).map(|i| self.entry(i))
    }

    fn with_refs(&self) -> impl Iterator<Item = (&Entry, &AtomicI32)> {
        self.entries().zip(self.refs.iter())
    }

    /// @nr slots of @stride bytes, zeroed, their refs 0.
    fn zeroed(nr: usize, stride: usize) -> Result<Self, AllocError> {
        let mut refs = KVVec::with_capacity(nr, flags::GFP_KERNEL)?;
        for _ in 0..nr {
            refs.push(AtomicI32::new(0), flags::GFP_KERNEL)?;
        }
        Ok(ReplicasCpu { entries: StridedBuf::zeroed(nr, stride)?, refs })
    }

    /// The entries @walk yields that @keep keeps, each one's devices sorted -
    /// the table not.
    fn from_entries<'a, I: Iterator<Item = SbEntry<'a>>>(walk: impl Fn() -> I, keep: impl Fn(u8) -> bool)
        -> Result<Self, AllocError>
    {
        let kept = || walk().filter(|e| keep(e.data_type));
        let mut r = Self::zeroed(kept().count(), kept().map(|e| DEVS + e.devs.len()).max().unwrap_or(0))?;

        for (i, e) in kept().enumerate() {
            let s = r.entries.slot_mut(i);
            s[..DEVS].copy_from_slice(&[e.data_type, e.devs.len() as u8, e.nr_required.unwrap_or(1)]);
            let devs = &mut s[DEVS..DEVS + e.devs.len()];
            devs.copy_from_slice(e.devs);
            devs.sort_unstable();
        }
        Ok(r)
    }

    fn sort(&mut self) {
        Eytzinger0::<usize>::new(self.len()).sort(self,
            |r, i, j| r.entries.slot(i).cmp(r.entries.slot(j)),
            |r, i, j| {
                r.entries.swap(i, j);
                r.refs.swap(i, j);
            });
    }

    /// The slot holding @e, if any.
    fn find(&self, e: &Entry) -> Option<usize> {
        verify_entry(e);
        let want = bytes_of(e);
        if want.len() > self.entries.stride() {
            return None;
        }
        Eytzinger0::<usize>::new(self.len()).find(|i| self.entries.slot(i)[..want.len()].cmp(want))
    }

    /// A copy, @e added - with room for it, if it's bigger than any: sorted.
    fn with(&self, e: &Entry) -> Result<Self, AllocError> {
        let e = bytes_of(e);
        let mut new = ReplicasCpu {
            entries: self.entries.restrided(self.entries.stride().max(e.len()), 1)?,
            refs:    KVVec::with_capacity(self.len() + 1, flags::GFP_KERNEL)?,
        };
        for r in self.refs.iter() {
            new.refs.push(AtomicI32::new(r.load(Ordering::Relaxed)), flags::GFP_KERNEL)?;
        }
        new.refs.push(AtomicI32::new(0), flags::GFP_KERNEL)?;
        new.entries.slot_mut(self.len())[..e.len()].copy_from_slice(e);
        new.sort();
        Ok(new)
    }

    /// A copy of those @keep says to: sorted.
    fn filtered(&self, keep: impl Fn(&Entry) -> bool) -> Result<Self, AllocError> {
        let mut new = Self::zeroed(self.entries().filter(|e| keep(e)).count(), self.entries.stride())?;
        for (dst, i) in (0..self.len()).filter(|&i| keep(self.entry(i))).enumerate() {
            new.entries.slot_mut(dst).copy_from_slice(self.entries.slot(i));
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
    fn to_sb(&self, sb: &mut c::bch_sb_handle) -> bool {
        /// Section @F, resized, the other one deleted: its entries' bytes.
        fn section<'a, F: SbField + Unsized + ?Sized + 'a>(sb: &'a mut c::bch_sb_handle, u64s: u32,
                                                         other: c::bch_sb_field_type) -> Option<&'a mut [u8]> {
            sb.field_resize::<F>(u64s)?;
            sb.field_delete(other);
            Some(&mut bytes_of_mut(sb.field_mut::<F>().expect("just resized"))[SECTION_HEAD..])
        }

        /// @e as the section has it: v0's entries have no nr_required, the
        /// byte before the devices.
        fn parts(e: &Entry, v1: bool) -> [&[u8]; 2] {
            let b = bytes_of(e);
            if v1 { [b, &[]] } else { [&b[..DEVS - 1], &b[DEVS..]] }
        }

        let v1 = self.entries().any(|e| e.nr_required != 1);

        let bytes = SECTION_HEAD + self.entries().flat_map(|e| parts(e, v1)).map(<[u8]>::len).sum::<usize>();
        let u64s = bytes.div_ceil(size_of::<u64>()) as u32;
        let out = if v1 {
            section::<c::bch_sb_field_replicas>(sb, u64s, c::bch_sb_field_type::BCH_SB_FIELD_replicas_v0)
        } else {
            section::<c::bch_sb_field_replicas_v0>(sb, u64s, c::bch_sb_field_type::BCH_SB_FIELD_replicas)
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
    fn write_sb(&self, fs: &Fs, sb: &mut c::bch_sb_handle) -> Result<(), BchError> {
        fs.ensure(self.to_sb(sb), bch_errcode::BCH_ERR_ENOSPC_sb_replicas)
    }

    /// Validated as a section is, its entries against @sb's members: none
    /// twice.
    fn validate<S: SbState>(&mut self, sb: &c::bch_sb<S>, err: &mut Printbuf) -> Result<(), BchError> {
        self.sort();

        let mut prev: Option<usize> = None;
        for i in Eytzinger0::<usize>::new(self.len()).iter() {
            let e = self.entry(i);
            e.validate(|d| member_exists(sb, d.into()), err)?;

            if let Some(p) = prev.filter(|&p| self.entries.slot(p) == self.entries.slot(i)) {
                write!(err, "duplicate replicas entry ");
                self.entry(p).to_text(err);
                return Err(bch_errcode::BCH_ERR_invalid_sb_replicas.into());
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
        assert!(e.devs.windows(2).all(|w| u32::from(w[0]) == c::BCH_SB_MEMBER_INVALID || w[0] < w[1]));
    }
}

/// A section's entries, @walk, validated as the section: against @sb's
/// members, none twice.
fn section_validate<'a, I: Iterator<Item = SbEntry<'a>>>(sb: &c::bch_sb<Unvalidated>, walk: impl Fn() -> I,
                                                         err: &mut Printbuf) -> Result<(), BchError> {
    ReplicasCpu::from_entries(walk, |_| true)
        .map_err(|_| BchError::from(bch_errcode::BCH_ERR_ENOMEM_cpu_replicas))?
        .validate(sb, err)
}

fn section_to_text<'a>(out: &mut Printbuf, entries: impl Iterator<Item = SbEntry<'a>>) {
    prt_spaced(out, entries, |out, e| e.to_text(out));
    out.newline();
}

impl SbFieldOps for c::bch_sb_field_replicas {
    fn validate(sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_replicas<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        section_validate(sb, || v1_entries_as_read(f.as_bytes()), err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        section_to_text(out, v1_entries(f))
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<Entry>(b, SECTION_HEAD, |e| e.data_type != 0)
    }
}

impl SbFieldOps for c::bch_sb_field_replicas_v0 {
    fn validate(sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_replicas_v0<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        section_validate(sb, || v0_entries_as_read(f.as_bytes()), err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        section_to_text(out, v0_entries(f))
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<c::bch_replicas_entry_v0<Validated>>(b, SECTION_HEAD, |e| e.data_type != 0)
    }
}

// ── Marking, refs, gc ────────────────────────────────────────────────────

impl Fs {
    /// @r's slot @i dropped, and the superblock's entries rewritten without
    /// it - not written: the caller's.
    fn replicas_kill(&self, r: &mut ReplicasCpu, sb: &mut c::bch_sb_handle, i: usize) {
        r.remove(i);
        if let Err(e) = r.write_sb(self, sb) {
            warn(self, format_args!("bch2_cpu_replicas_to_sb_replicas() error: {e}"));
        }
    }

    /// Whether entries of @data_type have no place in the superblock: user
    /// data's don't, past no_sb_user_data_replicas.
    fn replicas_obsolete(&self, data_type: u8) -> bool {
        u32::from(data_type) > c::bch_data_type::BCH_DATA_btree.0 &&
            u32::from(self.sb.version_incompat.get()) >= c::bcachefs_metadata_version::no_sb_user_data_replicas.0
    }

    /// Whether @e is marked in the superblock - or needn't be: obsolete, or
    /// of no devices.
    pub fn replicas_marked(&self, e: &Entry) -> bool {
        self.replicas_obsolete(e.data_type) || e.devs.is_empty() || self.replicas.read().find(e).is_some()
    }

    /// @e added, if it isn't there, and written to the superblock - and
    /// @refs taken on it.
    #[inline(never)]
    fn mark_replicas_slowpath(&self, e: &Entry, refs: i32) -> Result<(), BchError> {
        verify_entry(e);

        let mut sb = self.sb_lock();
        let dirty = {
            let mut r = self.replicas.write();
            let mut dirty = false;
            if r.find(e).is_none() {
                let new = r.with(e).map_err(|_| self.err(bch_errcode::BCH_ERR_ENOMEM_cpu_replicas))?;
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
        if self.replicas_marked(e) { Ok(()) } else { self.mark_replicas_slowpath(e, 0) }
    }

    /// Whether @e, a journal entry, has devices: only those are refcounted.
    fn journal_ref_entry(e: &Entry) -> bool {
        if e.devs.is_empty() {
            return false;
        }
        assert_eq!(u32::from(e.data_type), c::bch_data_type::BCH_DATA_journal.0,
                   "a ref on replicas entry of data type {}", e.data_type);
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
            let changed = r.retain(|e, refs| u32::from(e.data_type) != c::bch_data_type::BCH_DATA_journal.0 ||
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
        unsafe { c::bch2_accounting_mem_gc(self.as_ptr()) };

        let mut sb = self.sb_lock();
        {
            let _mark = self.mark_lock().write();
            let mut r = self.replicas.write();

            let new = r.filtered(|e| u32::from(e.data_type) == c::bch_data_type::BCH_DATA_journal.0 ||
                    // SAFETY: an entry; under mark_lock
                    unsafe { c::bch2_accounting_mem_has_replicas(self.as_ptr(), core::ptr::from_ref(e).cast()) })
                .map_err(|_| {
                    crate::bch_err!(self, "error allocating c->replicas_gc");
                    self.err(bch_errcode::BCH_ERR_ENOMEM_replicas_gc)
                })?;
            new.write_sb(self, &mut sb)?;
            *r = new;
        }
        self.write_super(&mut sb);
        Ok(())
    }

    /// Whether the filesystem can be read with @devs: every entry, as
    /// can_read_with_devs().
    pub fn can_read_fs_with_devs(&self, devs: &c::bch_devs_mask, flags: u32, mut err: Option<&mut Printbuf>) -> bool {
        self.replicas.read().entries()
            .filter(|e| !self.replicas_obsolete(e.data_type))
            .all(|e| can_read_with_devs(e, devs, flags, err.as_deref_mut()))
    }
}

/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_replicas_marked(c: &Opaque<c::bch_fs>, e: *const Head<c::bch_replicas_entry_v1>) -> bool {
    // SAFETY: the caller's
    Fs::from_c(c).replicas_marked(unsafe { from_ffi(e) })
}

/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_mark_replicas(c: &Opaque<c::bch_fs>, e: *const Head<c::bch_replicas_entry_v1>) -> c_int {
    // SAFETY: the caller's
    ret_to_c(Fs::from_c(c).mark_replicas(unsafe { from_ffi(e) }))
}

/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_replicas_entry_get(c: &Opaque<c::bch_fs>, e: *const Head<c::bch_replicas_entry_v1>) -> c_int {
    // SAFETY: the caller's
    ret_to_c(Fs::from_c(c).replicas_entry_get(unsafe { from_ffi(e) }))
}

/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_replicas_entry_put_many(c: &Opaque<c::bch_fs>, e: *const Head<c::bch_replicas_entry_v1>,
                                           nr: c_uint) {
    // SAFETY: the caller's
    Fs::from_c(c).replicas_entry_put_many(unsafe { from_ffi(e) }, nr)
}

/// @e dropped - the accounting's last for it gone: under sb_lock and
/// mark_lock, for write, which the caller holds, writing the superblock after
/// it drops mark_lock.
///
/// # Safety
/// @e is an entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_replicas_entry_kill(c: &Opaque<c::bch_fs>, e: *const Head<c::bch_replicas_entry_v1>) {
    let fs = Fs::from_c(c);
    // SAFETY: the caller's
    let e = unsafe { from_ffi::<Entry>(e) };
    fs.mark_lock_assert_held();
    // SAFETY: C calls it under sb_lock
    let mut sb = unsafe { fs.sb_lock_held() };

    let mut r = fs.replicas.write();
    match r.find(e) {
        Some(i) => fs.replicas_kill(&mut r, &mut sb, i),
        None => warn(fs, format_args!("replicas entry not found in sb: {e}")),
    }
}

#[c_export]
pub fn bch2_replicas_gc_reffed(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(Fs::from_c(c).replicas_gc_reffed())
}

#[c_export]
pub fn bch2_replicas_gc_accounted(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(Fs::from_c(c).replicas_gc_accounted())
}

/// The table, from the superblock: at open - after, it would lose the refs.
pub(crate) fn sb_replicas_to_cpu(fs: &Fs, sb: &SbLockGuard) -> Result<(), BchError> {
    assert!(!fs.flag(c::bch_fs_flags::BCH_FS_started), "replicas table rebuilt with refs held");

    let mut new = ReplicasCpu::from_entries(|| sb_entries(sb.sb()), |t| !fs.replicas_obsolete(t))
        .map_err(|_| BchError::from(bch_errcode::BCH_ERR_ENOMEM_cpu_replicas))?;
    new.sort();
    *fs.replicas.write() = new;
    Ok(())
}

#[c_export]
pub fn bch2_can_read_fs_with_devs(c: &Opaque<c::bch_fs>, devs: &c::bch_devs_mask, flags: c_uint,
                                  err: Option<&mut Printbuf>) -> bool {
    Fs::from_c(c).can_read_fs_with_devs(devs, flags, err)
}

/// A warning for any ref left: at shutdown, the journal done with them all.
#[c_export]
pub fn bch2_verify_replicas_refs_clean(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    for (e, r) in fs.replicas.read().with_refs().filter(|(_, r)| r.load(Ordering::Relaxed) != 0) {
        warn(fs, format_args!("replicas entry ref leaked:\nref={} {e}", r.load(Ordering::Relaxed) as u32));
    }
}

/// The table emptied: for an image stripped of its replicas sections.
#[c_export]
pub fn bch2_replicas_clear(c: &Opaque<c::bch_fs>) {
    *Fs::from_c(c).replicas.write() = ReplicasCpu::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOURNAL: u8 = c::bch_data_type::BCH_DATA_journal.0 as u8;
    const BTREE: u8 = c::bch_data_type::BCH_DATA_btree.0 as u8;
    const USER: u8 = c::bch_data_type::BCH_DATA_user.0 as u8;

    fn bytes(data_type: u8, nr_required: u8, devs: &[u8]) -> Vec<u8> {
        [&[data_type, devs.len() as u8, nr_required][..], devs].concat()
    }

    fn entry(b: &[u8]) -> &Entry {
        // SAFETY: an entry, as long as its nr_devs says
        unsafe { from_ffi::<Entry>(b.as_ptr().cast()) }
    }

    fn same(a: &ReplicasCpu, b: &ReplicasCpu) -> bool {
        a.len() == b.len() && a.entries().all(|e| b.find(e).is_some())
    }

    /// What's added is found, what's removed isn't, and an entry that's
    /// another's prefix isn't it; the superblock gets them back - as v0
    /// while every nr_required is 1.
    #[test]
    fn table() {
        let es = [bytes(JOURNAL, 1, &[0, 1]), bytes(BTREE, 1, &[2]), bytes(USER, 1, &[0, 1, 2]),
                  bytes(BTREE, 1, &[0, 1])];
        let mut r = ReplicasCpu::new();
        for e in &es {
            r = r.with(entry(e)).unwrap();
        }
        assert!(es.iter().all(|e| r.find(entry(e)).is_some()));
        assert!(r.find(entry(&bytes(BTREE, 1, &[0]))).is_none());

        r.remove(r.find(entry(&es[1])).unwrap());
        assert!(r.find(entry(&es[1])).is_none());
        assert!([&es[0], &es[2], &es[3]].iter().all(|e| r.find(entry(e)).is_some()));

        let mut sb = c::bch_sb_handle::default();
        sb.sb_realloc(0).unwrap();
        let back = |sb: &c::bch_sb_handle| {
            let mut b = ReplicasCpu::from_entries(|| sb_entries(sb.sb()), |_| true).unwrap();
            b.sort();
            b
        };

        assert!(r.to_sb(&mut sb));
        assert!(sb.field::<c::bch_sb_field_replicas_v0>().is_some());
        assert!(same(&r, &back(&sb)));

        r = r.with(entry(&bytes(USER, 2, &[0, 1, 2]))).unwrap();
        assert!(r.to_sb(&mut sb));
        assert!(sb.field::<c::bch_sb_field_replicas>().is_some());
        assert!(sb.field::<c::bch_sb_field_replicas_v0>().is_none());
        assert!(same(&r, &back(&sb)));
    }

    /// A section listing an entry twice is invalid.
    #[test]
    fn duplicate() {
        let invalid = c::BCH_SB_MEMBER_INVALID as u8;
        let mut sb = c::bch_sb_handle::default();
        sb.sb_realloc(0).unwrap();

        let e = bytes(BTREE, 1, &[invalid]);
        let mut r = ReplicasCpu::new().with(entry(&e)).unwrap();
        let mut err = Printbuf::new();
        assert!(r.validate(sb.sb(), &mut err).is_ok());

        let mut r = r.with(entry(&e)).unwrap();
        assert!(r.validate(sb.sb(), &mut err).unwrap_err().matches(bch_errcode::BCH_ERR_invalid_sb_replicas));
    }

    /// An entry cut short at a section's end - C's walk stopped there - is
    /// dropped at read with a warning, the rest of its u64 zeroed; at write,
    /// it's invalid.
    #[test]
    fn partial_entry() {
        type V0 = c::bch_sb_field_replicas_v0;
        let validate = |write: bool| {
            let invalid = c::BCH_SB_MEMBER_INVALID as u8;
            let mut sb = c::bch_sb_handle::default();
            sb.sb_realloc(0).unwrap();
            assert!(ReplicasCpu::new().with(entry(&bytes(BTREE, 1, &[invalid]))).unwrap().to_sb(&mut sb));

            // a u64 more: past the whole entry, 3 bytes, the start of one
            // claiming 20 devices
            let f = sb.field_resize::<V0>(3).unwrap();
            bytes_of_mut(f)[SECTION_HEAD + 3..][..2].copy_from_slice(&[BTREE, 20]);

            let flags = if write { c::bch_validate_flags::BCH_VALIDATE_write } else { c::bch_validate_flags::empty() };
            let f = unsafe { crate::sb::io::bch2_sb_field_get_id(sb.sb, V0::FIELD_TYPE) };
            let mut err = Printbuf::new();
            let ret = crate::errcode::ret_to_c(unsafe { crate::sb::io::validate_as::<V0>(sb.sb, f, flags, &mut err) });
            (sb, ret, err)
        };

        let (sb, ret, err) = validate(false);
        assert_eq!((ret, err.as_str()), (0, "8 bytes past its last whole entry, dropped"));
        let f = sb.field::<V0>().unwrap();
        assert_eq!(f.as_bytes().len(), 16);
        assert!(f.as_bytes()[SECTION_HEAD + 3..].iter().all(|&b| b == 0));
        assert_eq!(sb_entries(sb.sb()).count(), 1);

        let (_, ret, err) = validate(true);
        assert!(ret != 0);
        assert_eq!(err.as_str(), "13 bytes past its last whole entry");
    }
}
