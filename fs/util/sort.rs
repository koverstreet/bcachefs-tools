// SPDX-License-Identifier: GPL-2.0

//! A sort for the kernel stack. core's sort_unstable() is a quicksort with a
//! scratch buffer of elements in every level of its recursion: for a big
//! element - a backpointer key - a frame's 3.5k, and a few levels
//! overflow a 16k kernel stack. heapsort_by() is the kernel's sort(): in
//! place, no recursion, O(n log n), not stable.

use core::cmp::Ordering;

/// @v sorted by @cmp, as sort_unstable_by().
pub fn heapsort_by<T>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) {
    let mut sift_down = |v: &mut [T], mut node: usize| loop {
        let mut child = 2 * node + 1;
        if child >= v.len() {
            break;
        }
        if child + 1 < v.len() && cmp(&v[child], &v[child + 1]) == Ordering::Less {
            child += 1;
        }
        if cmp(&v[node], &v[child]) != Ordering::Less {
            break;
        }
        v.swap(node, child);
        node = child;
    };

    for i in (0..v.len() / 2).rev() {
        sift_down(v, i);
    }
    for end in (1..v.len()).rev() {
        v.swap(0, end);
        sift_down(&mut v[..end], 0);
    }
}

#[cfg(test)]
mod tests {
    use super::heapsort_by;

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
