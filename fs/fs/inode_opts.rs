// SPDX-License-Identifier: GPL-2.0

use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use core::ffi::CStr;

// ── Inode options ────────────────────────────────────────────────────────
//
// BCH_INODE_OPTS(): the options an inode can set for itself and what's
// below it. On the inode they're stored with a +1 bias, so that 0 means
// "not set" - inherited - and 1 an explicit "none"; bi_fields_set says
// which were set on the inode itself, rather than propagated from a parent.

/// An inode option: an inode_opt_id below Inode_opt_nr.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct InodeOpt(c::inode_opt_id);

impl InodeOpt {
    pub fn from_index(i: u32) -> Option<InodeOpt> {
        // The enum is repr(u32), with a variant for every value below
        // Inode_opt_nr:
        (i < c::inode_opt_id::Inode_opt_nr.0)
            .then(|| InodeOpt(unsafe { core::mem::transmute::<u32, c::inode_opt_id>(i) }))
    }

    pub fn all() -> impl Iterator<Item = InodeOpt> {
        (0..c::inode_opt_id::Inode_opt_nr.0)
            .map(|i| InodeOpt::from_index(i).expect("below Inode_opt_nr"))
    }

    pub fn id(self) -> c::inode_opt_id {
        self.0
    }

    pub fn name(self) -> &'static CStr {
        unsafe { CStr::from_ptr((*c::bch2_inode_opts.as_ptr().add(self.0.0 as usize)).cast()) }
    }

    /// Its value on @inode, biased: 0 for not set.
    pub fn get(self, inode: &c::bch_inode_unpacked) -> u64 {
        macro_rules! get {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
                if self.0 == c::inode_opt_id::[<Inode_opt_ $name>] {
                    return inode.[<bi_ $name>] as u64;
                }
            )* } };
        }
        c::BCH_INODE_OPTS!(get);
        panic!("inode option {} out of range", self.0.0)
    }

    /// @v biased, 0 for not set.
    pub fn set(self, inode: &mut c::bch_inode_unpacked, v: u64) {
        macro_rules! set {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
                if self.0 == c::inode_opt_id::[<Inode_opt_ $name>] {
                    inode.[<bi_ $name>] = v as _;
                    return;
                }
            )* } };
        }
        c::BCH_INODE_OPTS!(set);
        panic!("inode option {} out of range", self.0.0)
    }

    /// Whether it's set on @inode itself, not inherited: bi_fields_set.
    pub fn is_own(self, inode: &c::bch_inode_unpacked) -> bool {
        inode.bi_fields_set & (1 << self.0.0) != 0
    }

    pub fn set_own(self, inode: &mut c::bch_inode_unpacked, own: bool) {
        let bit = 1 << self.0.0;
        if own {
            inode.bi_fields_set |= bit;
        } else {
            inode.bi_fields_set &= !bit;
        }
    }
}

/// Those @inode sets, with the +1 bias removed.
pub fn inode_opts_to_opts(inode: &c::bch_inode_unpacked) -> c::bch_opts {
    // Only reads @inode: C's signature isn't const.
    unsafe { c::bch2_inode_opts_to_opts(inode as *const _ as *mut _) }
}

/// @dst takes on @src's inode options - those it didn't set itself, and
/// casefolding only for a directory - as when it moves into directory @src.
/// Whether anything changed.
pub fn reinherit_attrs(dst: &mut c::bch_inode_unpacked, src: &c::bch_inode_unpacked) -> bool {
    // Only reads @src: C's signature isn't const.
    unsafe { c::bch2_reinherit_attrs(dst, src as *const _ as *mut _) }
}

/// What changing an inode's options leaves for after the commit: a reconcile
/// scan, if the options its data goes by changed, and pushing them up to the
/// inode's older snapshots - the propagate logged op.
impl c::inode_opt_change {
    pub fn new() -> Self {
        let mut ch = Self::default();
        unsafe { c::bch2_inode_opt_change_init(&mut ch) };
        ch
    }

    /// @inode's options have changed, in @snapshot, from those that gave
    /// reconcile options @old: if those changed, schedule the scan and start
    /// the propagate.
    pub fn record(
        &mut self,
        t:        &TransAttempt<'_, '_>,
        old:      &c::bch_extent_reconcile,
        inode:    &c::bch_inode_unpacked,
        snapshot: u32,
    ) -> Result<(), BchError> {
        // Only reads @old and @inode: C's signature isn't const.
        ret_to_result(unsafe {
            c::bch2_inode_opt_change_trans(t.raw(), old as *const _ as *mut _,
                                           inode as *const _ as *mut _, snapshot, self)
        })
    }

    /// After the commit: finish the propagate, if one was started, and wake
    /// reconcile, if there's a scan for it.
    pub fn finish(&mut self, trans: &BtreeTrans<'_>) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_inode_opt_change_finish(trans.raw(), self) })
    }
}
