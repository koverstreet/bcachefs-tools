// SPDX-License-Identifier: GPL-2.0

//! The open filesystems - C's bch2_fs_list - and the ones being opened.
//!
//! - Key: what a filesystem is found by: the devices an opener names, or -
//!   once their superblocks are read - a multi-device filesystem's UUID.
//!   Copies of a single-device image share a UUID, and may be open at once.
//! - Opening: a filesystem being opened, by a mount or an offline fsck: the
//!   Holder its devices are claimed for, and its devices, then its UUID.
//! - FsHandle: what an opener gets for its devices, from FsHandle::get(): the
//!   filesystem, already open, or an Opening, making the caller the one to
//!   open it.
//!
//! A mount that finds another mount's Opening waits for it to end, then gets
//! the filesystem it opened; any other conflict is EBUSY.

use core::ffi::CStr;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{bch_errcode, ret_to_result_void, BchError};
use crate::fs::{Claimed, Fs, OpenFs};
use crate::locking::Locked;
use crate::sb::io::Holder;
use crate::util::alloc::{flags::GFP_KERNEL, Arc, KVVec};
use crate::util::backref::BackRef;
use crate::util::futex::EventCount;
use crate::util::log::{log_no_fs, LogLevel};
use crate::util::printbuf::{Printbuf, ToText};
use crate::util::rcu::read_lock;
use crate::util::sleep::Sleep;
use crate::prt;
use cstruct_macros::c_export;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Opener {
    Mount,
    Fsck,
}

impl Opener {
    /// Only mounts share a filesystem, open or being opened
    fn shares(self) -> bool {
        self == Opener::Mount
    }

    fn name(self) -> &'static str {
        match self {
            Opener::Mount => "a mount",
            Opener::Fsck => "an offline fsck",
        }
    }
}

#[derive(Clone, Copy)]
pub enum Key<'a> {
    Devs(&'a [c::dev_t]),
    Uuid([u8; 16]),
}

/// @paths' devices, by number: no IO, nothing claimed
pub fn devs<'a>(paths: impl Iterator<Item = &'a CStr>) -> Result<KVVec<c::dev_t>, BchError> {
    let mut devs = KVVec::new();
    for path in paths {
        let mut dev = 0;
        ret_to_result_void(unsafe { c::lookup_bdev(path.as_ptr().cast(), &mut dev) }).inspect_err(|e| {
            log_no_fs(LogLevel::ERR, crate::message!("cannot open with device {}: {}",
                                                     path.to_str().unwrap_or("?"), e.msg()));
        })?;
        devs.push(dev, GFP_KERNEL)?;
    }
    if devs.is_empty() {
        return Err(c::EINVAL.into());
    }
    Ok(devs)
}

impl ToText for Key<'_> {
    fn to_text(&self, out: &mut Printbuf) {
        match *self {
            Key::Devs(devs) => {
                prt!(out, "on device{}", if devs.len() > 1 { "s" } else { "" });
                for (i, dev) in devs.iter().enumerate() {
                    prt!(out, "{}{}:{}", if i > 0 { ", " } else { " " },
                         dev >> c::MINORBITS, dev & ((1 << c::MINORBITS) - 1));
                }
            }
            Key::Uuid(b) => c::__uuid_t { b }.to_text(out),
        }
    }
}
crate::display_via_to_text!(impl['a] Key<'a>);

struct OpeningEntry {
    holder: Arc<Holder>,
    by:     Opener,
    devs:   KVVec<c::dev_t>,
    uuid:   Option<[u8; 16]>,
}

struct Registry {
    open:    KVVec<BackRef<Fs>>,
    opening: KVVec<OpeningEntry>,
}

// SAFETY: initialized by bch2_fs_registry_init(), at module init
static REGISTRY: Locked<c::mutex, Registry> =
    unsafe { Locked::uninit(Registry { open: KVVec::new(), opening: KVVec::new() }, ()) };

/// Woken when an Opening ends
static OPENING_DONE: EventCount = EventCount::new();

fn fs_uuid(fs: &Fs) -> [u8; 16] {
    fs.sb.uuid.each_ref().map(|b| b.get())
}

enum Found {
    Free,
    Open(NonNull<Fs>),
    /// Another mount is opening it
    Busy,
}

impl Registry {
    fn find(&self, key: Key<'_>) -> Option<NonNull<Fs>> {
        let rcu = read_lock();
        let fs = self.open.iter().find(|fs| match key {
            Key::Devs(devs) => devs.iter().all(|&dev| {
                fs.devs_rcu(&rcu).any(|ca| !ca.disk_sb.bdev.is_null() && ca.dev == dev)
            }),
            Key::Uuid(uuid) => fs.sb.multi_device.get() && fs_uuid(fs) == uuid,
        });
        fs.map(|fs| NonNull::from(&**fs))
    }

    /// What @key finds for @by, but @ours - or EBUSY
    fn lookup(&self, key: Key<'_>, by: Opener, ours: Option<&Arc<Holder>>) -> Result<Found, BchError> {
        let other = self.opening.iter()
            .filter(|o| ours.is_none_or(|h| !Arc::ptr_eq(&o.holder, h)))
            .find(|o| match key {
                Key::Devs(devs) => o.devs.iter().any(|d| devs.contains(d)),
                Key::Uuid(uuid) => o.uuid == Some(uuid),
            });
        if let Some(other) = other {
            if !(by.shares() && other.by.shares()) {
                log_no_fs(LogLevel::ERR, crate::message!("filesystem {key} is being opened by {}",
                                                         other.by.name()));
                return Err(c::EBUSY.into());
            }
            return Ok(Found::Busy);
        }

        if let Some(fs) = self.find(key) {
            if !by.shares() {
                log_no_fs(LogLevel::ERR, crate::message!("filesystem {key} already open: mounted?"));
                return Err(c::EBUSY.into());
            }
            return Ok(Found::Open(fs));
        }
        Ok(Found::Free)
    }
}

fn wait_opening(seen: u32) -> Result<(), BchError> {
    OPENING_DONE.wait(seen, Sleep::KILLABLE).map_err(|_| c::EINTR.into())
}

pub enum FsHandle {
    /// Not pinned - it may be freed at any time - so only to be compared with
    /// the filesystem a superblock pins
    Open(NonNull<Fs>),
    Opening(Opening),
}

/// Ends when dropped
pub struct Opening {
    holder: Arc<Holder>,
    by:     Opener,
}

impl Drop for Opening {
    fn drop(&mut self) {
        REGISTRY.lock().opening.retain(|o| !Arc::ptr_eq(&o.holder, &self.holder));
        OPENING_DONE.wake();
    }
}

impl FsHandle {
    pub fn get(devs: &[c::dev_t], by: Opener) -> Result<FsHandle, BchError> {
        loop {
            let seen = OPENING_DONE.snapshot();
            let mut registry = REGISTRY.lock();

            match registry.lookup(Key::Devs(devs), by, None)? {
                Found::Open(fs) => return Ok(FsHandle::Open(fs)),
                Found::Busy => {
                    drop(registry);
                    wait_opening(seen)?;
                    continue;
                }
                Found::Free => {}
            }

            let holder = Holder::new()?;
            let mut d = KVVec::with_capacity(devs.len(), GFP_KERNEL)?;
            d.extend_from_slice(devs, GFP_KERNEL)?;
            registry.opening.push(OpeningEntry { holder: holder.clone(), by, devs: d, uuid: None }, GFP_KERNEL)?;
            return Ok(FsHandle::Opening(Opening { holder, by }));
        }
    }
}

pub enum Claim {
    /// Ours to open; the Opening ends when it's dropped
    Ours(Opening, Claimed),
    /// Its UUID already open: not pinned, as FsHandle::Open
    Open(NonNull<Fs>, [u8; 16]),
    /// Another mount was opening it, and has finished: look again
    Retry,
}

impl Opening {
    /// Its devices claimed and read: a multi-device filesystem is then found
    /// by its UUID too
    pub fn claim<'a>(self, devs: impl ExactSizeIterator<Item = &'a CStr>, opts: &mut c::bch_opts)
        -> Result<Claim, BchError>
    {
        let claimed = OpenFs::claim(devs, opts, &self.holder)?;
        let Some(uuid) = claimed.uuid() else { return Ok(Claim::Ours(self, claimed)) };

        let seen = OPENING_DONE.snapshot();
        let mut registry = REGISTRY.lock();
        match registry.lookup(Key::Uuid(uuid), self.by, Some(&self.holder))? {
            Found::Open(fs) => Ok(Claim::Open(fs, uuid)),
            Found::Busy => {
                drop(registry);
                drop((claimed, self));
                wait_opening(seen)?;
                Ok(Claim::Retry)
            }
            Found::Free => {
                if let Some(o) = registry.opening.iter_mut().find(|o| Arc::ptr_eq(&o.holder, &self.holder)) {
                    o.uuid = Some(uuid);
                }
                Ok(Claim::Ours(self, claimed))
            }
        }
    }
}

/// Not pinned, as FsHandle::Open
pub fn find(key: Key<'_>) -> Option<NonNull<Fs>> {
    REGISTRY.lock().find(key)
}

#[c_export]
pub fn bch2_fs_registry_init() {
    // SAFETY: module init, once
    let Ok(()) = unsafe { REGISTRY.init_zeroed(c"REGISTRY", crate::new_lock!(@key)) };
}

pub fn register(fs: &Fs) -> Result<(), BchError> {
    let mut registry = REGISTRY.lock();

    if fs.sb.multi_device.get() && registry.open.iter().any(|f| fs_uuid(f) == fs_uuid(fs)) {
        return fs.throw(bch_errcode::BCH_ERR_filesystem_uuid_already_open);
    }
    // SAFETY: exit() deregisters it before freeing it
    Ok(registry.open.push(unsafe { BackRef::new(fs) }, GFP_KERNEL)?)
}

pub fn deregister(fs: &Fs) {
    REGISTRY.lock().open.retain(|f| !core::ptr::eq(&**f, fs));
}
