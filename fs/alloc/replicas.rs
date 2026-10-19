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
use crate::fs::Fs;
use crate::opts::prt_data_type;
use crate::sb::io::sb_field_get;
use crate::sb::members::member_exists;
use crate::util::ffi::Opaque;
use crate::util::rcu::read_lock;
use crate::util::vstructs::{from_ffi, from_ffi_mut, Head, Validated, ValidatedEntries};
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use core::fmt;
use core::mem::{offset_of, size_of};
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

impl Entry {
    pub fn to_text(&self, out: &mut Printbuf) {
        SbEntry::from(self).to_text(out)
    }

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

/// # Safety
/// @e is a v0 entry, as long as its nr_devs says.
#[c_export]
pub unsafe fn bch2_replicas_entry_v0_to_text(out: &mut Printbuf, e: *const Head<c::bch_replicas_entry_v0>) {
    // SAFETY: the caller's
    SbEntry::from(unsafe { from_ffi::<c::bch_replicas_entry_v0<Validated>>(e) }).to_text(out)
}

/// Against the members of @sb.
///
/// # Safety
/// @e is an entry, as long as its nr_devs says; @sb is validated.
#[c_export]
pub unsafe fn bch2_replicas_entry_sb_validate(e: *const Head<c::bch_replicas_entry_v1>,
                                              sb: *const Head<c::bch_sb>, err: &mut Printbuf) -> c_int {
    // SAFETY: the caller's
    let (e, sb) = unsafe { (from_ffi::<Entry>(e), c::bch_sb::<Validated>::from_ffi(sb)) };
    ret_to_c(e.validate(|d| member_exists(sb, d.into()), err))
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

