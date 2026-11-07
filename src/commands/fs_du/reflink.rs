//! Which files reference a range of the reflink btree. An indirect extent
//! keeps a refcount, not who holds it, so the answer takes every reflink_p
//! key: one walk of the extents btree across all snapshots, each kept as the
//! range it references and its (inode, snapshot).
//!
//! Sorted by range start, with a running maximum of the ends beside it: the
//! ranges overlapping a query are among those starting before its end, and
//! walking those back stops once the running maximum is at or below its start,
//! as no earlier one reaches into it.

use std::ops::Range;

use anyhow::Result;
use bcachefs_kernel::btree::bkey::{POS_MAX, POS_MIN};
use bcachefs_kernel::format::Btree;

use super::reflink_p_range;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

pub(super) struct ReflinkRefs {
    refs:    Vec<(Range<u64>, (u64, u32))>,
    max_end: Vec<u64>,
}

impl ReflinkRefs {
    pub(super) fn load(handle: &BcachefsHandle) -> Result<Self> {
        let mut refs = Vec::new();
        let mut iter = OnlineBtreeIter::new(handle, Btree::extents, 0, POS_MIN, POS_MAX,
                                            OnlineIterFlags::ALL_SNAPSHOTS);
        while let Some(k) = iter.next()? {
            if let Some(r) = reflink_p_range(k) {
                refs.push((r, (k.k.p.inode, k.k.p.snapshot)));
            }
        }
        Ok(Self::new(refs))
    }

    fn new(mut refs: Vec<(Range<u64>, (u64, u32))>) -> Self {
        refs.sort_by_key(|(r, _)| r.start);
        let max_end = refs.iter()
            .scan(0, |max, (r, _)| {
                *max = r.end.max(*max);
                Some(*max)
            })
            .collect();
        ReflinkRefs { refs, max_end }
    }

    /// The (inode, snapshot)s with a reflink_p overlapping @range, each once.
    pub(super) fn referencers(&self, range: &Range<u64>) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        for i in (0..self.refs.partition_point(|(r, _)| r.start < range.end)).rev() {
            if self.max_end[i] <= range.start {
                break;
            }
            let (r, owner) = &self.refs[i];
            if r.end > range.start && !out.contains(owner) {
                out.push(*owner);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Overlap is half-open, a long range early on is still found, and an
    /// inode referencing twice is one referencer.
    #[test]
    fn referencers_overlapping_a_range() {
        let refs = ReflinkRefs::new(vec![
            (0..100, (1, 1)),
            (10..20, (2, 1)),
            (20..30, (3, 1)),
            (40..50, (4, 1)),
            (45..60, (4, 1)),
            (200..210, (5, 2)),
        ]);
        let sorted = |r: Range<u64>| {
            let mut v = refs.referencers(&r);
            v.sort();
            v
        };

        assert_eq!(sorted(20..30), [(1, 1), (3, 1)]);
        assert_eq!(sorted(30..40), [(1, 1)]);
        assert_eq!(sorted(48..55), [(1, 1), (4, 1)]);
        assert_eq!(sorted(100..200), []);
        assert_eq!(sorted(205..206), [(5, 2)]);
    }
}
