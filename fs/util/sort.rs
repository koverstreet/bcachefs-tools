// SPDX-License-Identifier: GPL-2.0

//! A sort for the kernel stack. core's sort_unstable() is a quicksort with a
//! scratch buffer of elements in every level of its recursion: for a big
//! element - a backpointer key - a frame's 3.5k, and a few levels
//! overflow a 16k kernel stack. heapsort_by() is the kernel's sort(): in
//! place, no recursion, O(n log n), not stable.

use core::cmp::Ordering;

use super::heap::sift_down;

/// @v sorted by @cmp, as sort_unstable_by().
pub fn heapsort_by<T>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) {
    for i in (0..v.len() / 2).rev() {
        sift_down(v, i, &mut cmp);
    }
    for end in (1..v.len()).rev() {
        v.swap(0, end);
        sift_down(&mut v[..end], 0, &mut cmp);
    }
}

/// Vec::dedup_by(), on a slice: each element @same_bucket(it, kept) says
/// belongs with the last one kept is dropped - the callback can merge it in
/// first. The kept ones are moved to the front; their count is returned, to
/// truncate to.
pub fn dedup_by<T>(v: &mut [T], mut same_bucket: impl FnMut(&mut T, &mut T) -> bool) -> usize {
    if v.is_empty() {
        return 0;
    }

    let mut kept = 1;
    for i in 1..v.len() {
        let (head, tail) = v.split_at_mut(i);
        if !same_bucket(&mut tail[0], &mut head[kept - 1]) {
            v.swap(i, kept);
            kept += 1;
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::{dedup_by, heapsort_by};

    // A wrong dedup is silent: node scan would merge or drop the wrong
    // replicas.
    #[test]
    fn dedup_by_keeps_the_first_of_each_run() {
        let mut v = [(1, 1), (1, 2), (2, 3), (3, 4), (3, 5), (3, 6), (4, 7)];
        let kept = dedup_by(&mut v, |a, kept| {
            let same = a.0 == kept.0;
            if same {
                kept.1 += a.1;
            }
            same
        });
        assert_eq!(&v[..kept], [(1, 3), (2, 3), (3, 15), (4, 7)]);
    }

    // A wrong sort is silent: the backpointer scan would just walk out of
    // order.
    #[test]
    fn heapsort_by_sorts() {
        let mut s: u64 = 1;
        for n in 0..200 {
            let mut v: Vec<u32> = (0..n).map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (s >> 40) as u32 % 50
            }).collect();
            let mut want = v.clone();
            want.sort_unstable_by(|a, b| b.cmp(a));
            heapsort_by(&mut v, |a, b| b.cmp(a));
            assert_eq!(v, want, "n = {n}");
        }
    }
}
