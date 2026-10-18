//! Positions: bpos constructors, the sentinels, and stepping to the next or
//! previous position.

use crate::c;
use crate::c::bpos as Bpos;

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

impl c::bpos {
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
        let (inode, offset, snapshot) = (self.inode, self.offset, self.snapshot);

        if let Some(snapshot) = snapshot.checked_sub(1) {
            spos(inode, offset, snapshot)
        } else if let Some(offset) = offset.checked_sub(1) {
            spos(inode, offset, u32::MAX)
        } else {
            spos(inode.checked_sub(1).expect("no position before POS_MIN"), u64::MAX, u32::MAX)
        }
    }
}
