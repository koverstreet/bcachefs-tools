//! Comparing positions: Bpos's Ord - every field, C's bpos_*() - and
//! bkey_*() - inode and offset, the snapshot field ignored. Its Hash is of
//! every field too, as its Eq.

use core::cmp::Ordering;
use core::hash::{Hash, Hasher};
use crate::format::Bpos;

impl Hash for Bpos {
    fn hash<H: Hasher>(&self, h: &mut H) {
        // packed: the fields by value
        (self.inode, self.offset, self.snapshot).hash(h)
    }
}

impl PartialEq for Bpos {
    fn eq(&self, other: &Self) -> bool {
        (self.inode, self.offset, self.snapshot) == (other.inode, other.offset, other.snapshot)
    }
}

impl Eq for Bpos {}

impl PartialOrd for Bpos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bpos {
    fn cmp(&self, other: &Self) -> Ordering {
        /* not Ordering::then(): that compares every field, and < can't skip it */
        (self.inode, self.offset, self.snapshot).cmp(&(other.inode, other.offset, other.snapshot))
    }
}

#[inline(always)]
pub fn bkey_eq(l: Bpos, r: Bpos) -> bool {
    l.inode == r.inode && l.offset == r.offset
}

#[inline(always)]
pub fn bkey_lt(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset < r.offset
    }
}

#[inline(always)]
pub fn bkey_le(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset <= r.offset
    }
}

#[inline(always)]
pub fn bkey_gt(l: Bpos, r: Bpos) -> bool {
    bkey_lt(r, l)
}

#[inline(always)]
pub fn bkey_ge(l: Bpos, r: Bpos) -> bool {
    bkey_le(r, l)
}

#[inline]
pub fn bkey_min(l: Bpos, r: Bpos) -> Bpos {
    if bkey_lt(l, r) { l } else { r }
}

#[inline(always)]
pub fn bkey_cmp(l: Bpos, r: Bpos) -> i32 {
    if l.inode != r.inode {
        if l.inode < r.inode { -1 } else { 1 }
    } else if l.offset != r.offset {
        if l.offset < r.offset { -1 } else { 1 }
    } else {
        0
    }
}
