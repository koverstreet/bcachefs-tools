// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::{new_mutex, Mutex};
use crate::util::genradix::SharedGenRadix;
use core::sync::atomic::AtomicU32;
use cstruct_macros::{c_enum, CStruct, TypeInfo};
use pin_init::{pin_data, pin_init, PinInit};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_qid {
    pub q: [u32; c::QTYP_NR as usize],
}

c_enum! {
    #[closed]
    pub enum quota_acct_mode: u32 {
        KEY_TYPE_QUOTA_PREALLOC,
        KEY_TYPE_QUOTA_WARN,
        KEY_TYPE_QUOTA_NOCHECK,
    }
}

/// One counter of one id's in-memory quota: its usage, its limits, and the
/// grace period's end and warnings.
#[derive(Clone, Copy, Default, PartialEq, ::pin_init::Zeroable)]
pub struct MemquotaCounter {
    pub v:              u64,
    pub hardlimit:      u64,
    pub softlimit:      u64,
    pub timer:          i64,
    pub warns:          i32,
    pub warning_issued: u32,
}

/// One id's in-memory quota - a uid's, a gid's or a project's.
#[derive(Clone, Copy, Default, PartialEq, ::pin_init::Zeroable)]
pub struct Memquota {
    pub c: [MemquotaCounter; c::Q_COUNTERS as usize],
}

/// A limit from the superblock: written under sb_lock, read without a lock,
/// as C's are.
#[derive(Default)]
pub struct QuotaLimit {
    pub timelimit: AtomicU32,
    pub warnlimit: AtomicU32,
}

/// The in-memory quotas of one type, as C's struct bch_memquota_type: the
/// limits, and the counters by id - allocated without the lock, genradix
/// being safe against concurrent allocation and lookup, and touched only
/// with it held. quota.rs's guard is the way in.
#[pin_data]
pub struct MemquotaType {
    pub limits:       [QuotaLimit; c::Q_COUNTERS as usize],
    #[pin]
    pub(crate) lock:  Mutex<()>,
    pub(crate) table: SharedGenRadix<Memquota>,
}

impl MemquotaType {
    fn new(lock: impl PinInit<Mutex<()>>) -> impl PinInit<Self> {
        pin_init!(Self {
            limits:  Default::default(),
            lock    <- lock,
            table:   SharedGenRadix::new(),
        })
    }
}

/// The in-memory quotas, by type - c->quotas, Rust's alone: quota.rs.
#[pin_data]
pub struct Quotas {
    #[pin]
    usr: MemquotaType,
    #[pin]
    grp: MemquotaType,
    #[pin]
    prj: MemquotaType,
}

impl Quotas {
    /// No limits, and empty tables. A new_mutex!() each, so a lock class
    /// each: they're taken together, in type order, as C's are with
    /// mutex_lock_nested().
    pub fn new() -> impl PinInit<Self> {
        pin_init!(Self {
            usr <- MemquotaType::new(new_mutex!(())),
            grp <- MemquotaType::new(new_mutex!(())),
            prj <- MemquotaType::new(new_mutex!(())),
        })
    }

    /// Type @qtype's: a quota_types.
    pub fn get(&self, qtype: usize) -> &MemquotaType {
        match qtype {
            0 => &self.usr,
            1 => &self.grp,
            2 => &self.prj,
            _ => panic!("quota type {qtype} out of range"),
        }
    }
}
