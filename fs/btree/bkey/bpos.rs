//! Positions: bpos constructors, the sentinels, and stepping to the next or
//! previous position.

use crate::format::Bpos;

pub const fn spos(inode: u64, offset: u64, snapshot: u32) -> Bpos {
    Bpos {
        inode,
        offset,
        snapshot,
    }
}

pub const fn pos(inode: u64, offset: u64) -> Bpos {
    spos(inode, offset, 0)
}

pub const POS_MIN: Bpos = spos(0, 0, 0);
pub const POS_MAX: Bpos = spos(u64::MAX, u64::MAX, 0);
pub const SPOS_MAX: Bpos = spos(u64::MAX, u64::MAX, u32::MAX);

impl Bpos {
    /// `*self = *src`, a field at a time. FIXME rustc: assigning a Bpos is a
    /// 20 byte memcpy, lowered 8/8/4, and the fields are 4/8/8 - a field
    /// loaded soon after can't be forwarded from the two stores it straddles,
    /// and waits for them to commit: ~50 cycles on Zen 4, on an L1 hit.
    #[inline(always)]
    pub fn assign_fieldwise_fixme_rustc(&mut self, src: &Bpos) {
        self.snapshot = src.snapshot;
        self.offset   = src.offset;
        self.inode    = src.inode;
    }

    /// The next position, snapshot field first. None after SPOS_MAX: panics.
    pub fn successor(self) -> Self {
        let (inode, offset, snapshot) = (self.inode, self.offset, self.snapshot);

        if let Some(snapshot) = snapshot.checked_add(1) {
            spos(inode, offset, snapshot)
        } else if let Some(offset) = offset.checked_add(1) {
            spos(inode, offset, 0)
        } else {
            spos(inode.checked_add(1).expect("no position after SPOS_MAX"), 0, 0)
        }
    }

    /// The next position ignoring the snapshot field: past every snapshot
    /// version of this one. Panics after POS_MAX.
    pub fn nosnap_successor(self) -> Self {
        let (inode, offset) = (self.inode, self.offset);

        if let Some(offset) = offset.checked_add(1) {
            spos(inode, offset, 0)
        } else {
            spos(inode.checked_add(1).expect("no position after POS_MAX"), 0, 0)
        }
    }

    /// The previous position ignoring the snapshot field, at snapshot 0.
    /// Panics before POS_MIN.
    pub fn nosnap_predecessor(self) -> Self {
        let (inode, offset) = (self.inode, self.offset);

        if let Some(offset) = offset.checked_sub(1) {
            spos(inode, offset, 0)
        } else {
            spos(inode.checked_sub(1).expect("no position before POS_MIN"), u64::MAX, 0)
        }
    }

    /// The previous position, snapshot field first. None before POS_MIN:
    /// panics.
    pub fn predecessor(self) -> Self {
        self.checked_predecessor().expect("no position before POS_MIN")
    }

    /// predecessor(), or None at POS_MIN.
    pub fn checked_predecessor(self) -> Option<Self> {
        let (inode, offset, snapshot) = (self.inode, self.offset, self.snapshot);

        Some(if let Some(snapshot) = snapshot.checked_sub(1) {
            spos(inode, offset, snapshot)
        } else if let Some(offset) = offset.checked_sub(1) {
            spos(inode, offset, u32::MAX)
        } else {
            spos(inode.checked_sub(1)?, u64::MAX, u32::MAX)
        })
    }
}
