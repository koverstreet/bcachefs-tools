use crate::btree::bkey::BkeySC;
use crate::c;
use crate::errcode::{bch_errcode, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::kernel::{READ, WRITE};
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::vstructs::{Head, View};
use core::ffi::CStr;
use core::mem::offset_of;
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

const BCH_MEMBER_V1_BYTES: usize = 56;

/// bcachefs_format.h's BCH_SB_MEMBER_DELETED_UUID.
const MEMBER_DELETED_UUID: [u8; 16] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

impl c::bch_member {
    /// Not an empty slot or a deleted member.
    pub fn alive(&self) -> bool {
        let uuid = crate::util::vstructs::plain_bytes(&self.uuid);
        uuid != [0; 16] && uuid != MEMBER_DELETED_UUID
    }
}

/// From members_v2 if the superblock has it, else members_v1.
pub fn sb_member(sb: &c::bch_sb, idx: u32) -> Option<c::bch_member> {
    match members_v2(sb) {
        Some(v2) => v2.get(idx),
        None     => members_v1(sb)?.get(idx),
    }
}

/// Member @idx of a members section, its bytes @b: one of @member_bytes from
/// @start, copied - zero past the end of one shorter than bch_member. None
/// if it's past the section's end.
fn member_at(b: &[u8], start: usize, member_bytes: usize, idx: u32) -> Option<c::bch_member> {
    let at = start + idx as usize * member_bytes;
    Some(View::<c::bch_member>::new(b.get(at..at + member_bytes)?).get())
}

/// Read-only view of members_v2 with bounds-checked access.
pub struct MembersV2<'a> {
    /// The section's
    bytes: &'a [u8],
    member_bytes: usize,
    nr_devices: u32,
}

impl<'a> MembersV2<'a> {
    /// Get a copy of the member at `idx`, zero-extending if member_bytes
    /// is smaller than sizeof(bch_member).
    pub fn get(&self, idx: u32) -> Option<c::bch_member> {
        if idx >= self.nr_devices {
            return None;
        }
        member_at(self.bytes, offset_of!(Head<c::bch_sb_field_members_v2>, _members), self.member_bytes, idx)
    }

    pub fn member_bytes(&self) -> usize {
        self.member_bytes
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
    /// The section's
    bytes: &'a [u8],
    nr_devices: u32,
}

impl<'a> MembersV1<'a> {
    pub fn get(&self, idx: u32) -> Option<c::bch_member> {
        if idx >= self.nr_devices {
            return None;
        }
        member_at(self.bytes, offset_of!(Head<c::bch_sb_field_members_v1>, _members), BCH_MEMBER_V1_BYTES, idx)
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
    let field = sb_field_get::<c::bch_sb_field_members_v2>(sb)?;
    Some(MembersV2 {
        member_bytes: field.member_bytes.get() as usize,
        nr_devices: sb.nr_devices as u32,
        bytes: field.as_bytes(),
    })
}

/// Construct a MembersV1 reader from a superblock.
pub fn members_v1(sb: &c::bch_sb) -> Option<MembersV1<'_>> {
    let field = sb_field_get::<c::bch_sb_field_members_v1>(sb)?;
    Some(MembersV1 {
        nr_devices: sb.nr_devices as u32,
        bytes: field.as_bytes(),
    })
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

    /// bch2_dev_missing_atomic(): reported - from an atomic context, which is
    /// what it's safe from - and the error for it.
    fn dev_missing(&self, dev: u32) -> BchError {
        unsafe { c::bch2_dev_missing_atomic(self.as_ptr(), dev) };
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
            ret_to_result_void(unsafe { c::bch2_dev_missing_bkey(self.as_ptr(), k.into(), dev) })?;
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
            unsafe { c::bch2_dev_bucket_missing(ca.as_mut_ptr(), bucket.offset) };
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
